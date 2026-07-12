# Background Task Output Spool — 设计方案

> 状态：提案 v4（2026-07-11），尚未标记“架构评审通过”。v2 补齐 per-task
> 持久化串行化、强类型 TaskId、ring layout、session quota 和 pump failure；v3
> 补齐 fact-level notice identity、session-local inventory/corrupt accounting、
> fd-relative no-follow 文件操作和 crash-Running 的条件恢复语义；v4 补齐 terminal
> transition 后的 consumed cleanup 与 streaming/bounded inventory。本文是
> [`background-tasks.md`](./background-tasks.md) 的输出存储增量设计；任务执行、
> 进程组和 hub keepalive 仍以原文为准。本文新增 expiration fact，因此通知身份与恢复
> 去重部分以本文的 `TaskNoticeKey` 为准，取代原文按 `task_ids` 去重的协议。

## Problem

把 background task 的 stdout/stderr 从常驻内存尾缓冲迁移到 session-owned 的
有界磁盘环形文件，让长时间任务不会按输出上限持续占用进程内存，同时保证模型在
断线、hub entry release/reopen、正常 server 重启后仍能读取尚未消费的输出。

## Scope

**In：**

- 保留现有 stdout/stderr pipe pump，只把 pump 的目标从内存 `TailBuf` 改为磁盘
  环形文件；
- 每个 stream 具有固定文件容量、绝对逻辑偏移、增量读取游标和覆盖统计；
- hub-owned registry 的输出归 session 目录所有，并通过独立 task manifest 恢复；
- entry release、模型切换、断线和正常 server shutdown 不删除未读输出；
- `task_output` 分段读取，显式报告尚未读取便被覆盖的字节数；
- 完成通知显式报告文件曾覆盖多少历史输出；
- terminal task 从内存回收后仍可按 task id 从磁盘读取或查询终态；
- task 输出全部消费、session 删除或磁盘配额淘汰时的清理语义；
- session payload quota、淘汰顺序、淘汰通知和配额并发边界；
- completion/expiration/overflow batch 各自稳定的通知事实身份与恢复去重；
- entry reopen 的 session-local archive inventory、损坏项计费和 spawn blocker；
- 文件创建、写入、读取和 manifest 持久化失败的错误语义。

**Out：**

- 恢复 server 崩溃前仍在运行的进程，或重新接管孤儿进程；
- 把完整 stdout/stderr 序列化进 runtime checkpoint、thread checkpoint 或
  `pending_notices.json`；
- 永久日志归档、日志搜索、下载接口或向模型暴露本地文件路径；
- server-global stale scan 或启动/attach 时自动删除孤儿 spool；本版只做
  session-local inventory，接受异常退出残留；
- background task 的跨设备或远程日志存储；
- 改变当前“正常生命周期内通知恰好一次，崩溃时可能丢失或重复”的保证。

## Assumptions

- Unix-only 约束不变；task/output 目录必须为 `0700`，manifest/ring 文件必须为
  `0600`。archive 持有目录 fd/capability，子目录与文件只用 fd-relative
  `openat`/`renameat`/`unlinkat` 操作；open 使用 `O_NOFOLLOW`，随后对已打开 fd 做
  `fstat`。同一 Unix 用户并发替换 archive 路径不排除在 threat model 外。
- background task 的唯一增量输出消费者仍是本 session 的模型；dashboard 只读
  task summary，不推进游标。
- Running 上限仍为 16；live overview 仍只保留最近 32 个 terminal summary，但
  这个内存展示上限不再等同于磁盘输出的可读取期限。
- 每个 stream 初始容量仍为 512 KiB，以保持当前截尾和磁盘上界；容量是内部配置，
  不是 wire contract，后续可独立调大；每个文件必须使用 manifest 中创建时的容量。
- 支持的 stream capacity 下限为 64 KiB、上限为 64 MiB；manifest 超出范围按 corrupt
  处理。下限使 64 MiB session quota 下的 retained index 具有可证明上界。
- 每个 session 的 ring payload quota 为 64 MiB，按 manifest 中两个 stream 的
  capacity 之和预留；目录、manifest 和 filesystem metadata 的小量开销不计入 quota。
- active task 永不参与 quota 淘汰；`64 MiB >= MAX_RUNNING × 2 × 512 KiB`，保证当前
  配置下 16 个 active task 都能获得 reservation。配置加载必须验证这一不变量。
- 单次 `task_output` 每个 stream 最多返回 128 KiB；游标只推进实际返回的部分。
- `meta.json` 最大 64 KiB；inventory 在分配/read 前先看 opened-fd length，超限直接按
  corrupt 计费/阻塞。每条 issue sample 的 name/error 文本各截到 512 bytes。
- “完整输出”在本文中指“当前文件容量内仍保留的全部输出”。一旦环形文件覆盖旧
  内容，被覆盖字节不可恢复，但丢失必须可观察。
- 正常 release/shutdown 可以可靠地 flush、关闭文件并原子写 manifest；异常退出的
  文件和 manifest 一致性仍是 best-effort，不升级为事务保证。
- 破坏性 API、序列化和持久化格式变更可接受。

## Validation Findings

| 问题 | 方法 | 结果 | 设计含义 |
| --- | --- | --- | --- |
| 能否把 child stdout/stderr 直接重定向到文件？ | 检查 `run_process` 的 leader-exit / pipe-drain 状态机和回归测试 | 不能。leader 可先退出，后台后代仍持有 pipe；EOF 是任务是否真正排空的重要信号 | 保留 pipe pump，pump 再写 spool 文件 |
| 当前如何判断未读输出被截尾？ | 检查 `TailBuf::{start_offset,total_written}`、task cursor 和 `TaskRead::*_lost` | `lost = start_offset.saturating_sub(cursor)`；只有覆盖未读字节才算 consumer loss | 磁盘环形文件沿用绝对偏移模型，不能只保存物理文件位置 |
| entry release 后完整输出是否仍存在？ | 检查 hub idle release 和 registry ownership | 当前内存 registry 随 entry release 消失，只持久化通知的 4 KiB tail | 输出必须由 session 目录而非 entry 生命周期拥有 |
| terminal 内存回收是否可当作输出清理？ | 检查 `MAX_TERMINAL = 32` 的回收路径 | 第 33 个 terminal task 会从 registry map 移除 | 内存 summary 回收与磁盘 archive 清理必须解耦 |
| 通知投递是否等于输出已消费？ | 检查 driver 在下一用户 turn 注入 `TaskNotice` 的流程 | 通知先告诉模型任务完成，模型随后才可能调用 `task_output` | notice take/注入/checkpoint 都不能删除输出 |
| checkpoint 是否适合保存完整输出？ | 检查 runtime 多点整篇 checkpoint 写入和独立 `NoticeStore` 设计 | 不适合：文件会膨胀、重复改写，并引入 runtime 与 spool 双写者 | 只在独立 manifest 保存小型 task 元数据；输出字节只在 spool 文件中 |
| session 删除能否形成最终清理边界？ | 检查 `WorkspaceStorage::delete_session` | 会递归删除整个 session 目录 | hub-owned 输出放在 session 目录即可随 session 删除 |
| “registry 是 manifest 唯一写者”是否足以串行？ | 对照 `read`、monitor terminal commit、release 的并发路径 | 不足；这些路径都能在不持有 registry lock 时改写同一 manifest | 每 task 增加 commit lock，覆盖读取状态到 rename 和内存提交的完整事务 |
| 裸 task id 能否安全拼接路径？ | 对照模型可控的 `task_output.id` 与 `tasks_root.join(id)` | `../`、绝对路径等会逃出 archive root | 路径层只接受已验证的 `TaskId`，裸字符串只能在工具入口解析 |
| capacity 能否只用当前配置恢复？ | 对照 `logical_offset % capacity` | 配置变化会让旧逻辑偏移映射到错误物理位置 | 每 stream 持久化 `layout_version` 和创建时 `capacity`；reopen 不用新默认值 |
| session output 能否只靠 per-stream 上限保持有界？ | 计算未读 terminal task 的累积 | 不能；task 数可长期增长 | 增加 64 MiB session reservation quota，create 时串行淘汰最老 terminal，active 永不淘汰 |
| pump 写盘失败能否沿用当前 `JoinHandle<()>`？ | 检查 `run_process` 对 pump 的等待方式 | 不能；失败会和 EOF 一样消失，leader 还可能继续运行 | pump 返回结构化结果，主状态机同时监听 child/cancel/two pumps，failure kill group 并提交 Failed |
| completion 和 expiration 能否继续按 task id 去重？ | 对照 `UserOrigin::TaskNotice { task_ids }` 与 hub restore dedupe | 不能；同一 task 的旧 completion 会误删后产生的 expiration | 去重升级为稳定的 `TaskNoticeKey`，事实类型属于身份，status/time/reason 不属于身份 |
| corrupt/orphan 文件如何进入 quota？ | 对照 strict reopen validation 与“按 ring 重建 reservation” | 无可信 manifest 时不能忽略，也不能猜 capacity | entry reopen 做 session-local inventory；保守计费并设置 spawn blocker，attach 和其他有效 task 仍可用 |
| metadata check 能否防并发 symlink replacement？ | 检查 path metadata→open 两步 | 不能，存在 TOCTOU | archive 改用目录 fd + no-follow openat，并在已打开 fd 上验证类型/权限 |
| fully consumed 是否只可能由 read 触发？ | 交错 Running read 与 terminal transition | 不能；cursor 可先追平当时 total，随后 task 无新增输出直接结束，零输出 task 也天然追平 | terminal transition 后必须再次 finalize；quota victim recheck 遇到 fully consumed 必须优先 Consumed，禁止 Expired |
| inventory 能否返回所有 `Arc<TaskRecord>`？ | 令 Consumed/Expired metadata 长期累积 | 不能；reopen 内存/FD 与历史 task 总数线性增长，违背 lazy-load | streaming enumerate，每项检查后立即关 fd；只返回无 fd 的有界索引、最近 32 summary 和有界 issue samples |

## Components

1. **`DiskTail`（`coda_tools::background`）** — 用一个固定容量文件实现逻辑上连续
   的字节流，隐藏环形写入、跨 wrap 读取、绝对偏移和 tail 读取。
2. **`ArchiveDir`（`coda_tools::background`）** — 封装已打开的 session background
   目录 fd，集中提供 no-follow、fd-relative 的 enumerate/open/create/rename/unlink，
   调用方拿不到可自行 join/reopen 的裸路径。
3. **`TaskOutputFiles`（`coda_tools::background`）** — 拥有一个 task 的 stdout 和
   stderr 两个 `DiskTail`，并提供 task 级 flush/close/remove 操作。
4. **`TaskArchive`（`coda_tools::background`）** — 管理 task manifest、terminal
   task 的按 id 重新打开，以及内存回收与磁盘保留之间的边界；同一 TaskId 在进程内
   始终解析到同一个 live `TaskRecord`/commit lock；entry reopen 以 streaming scan
   生成无 fd 的有界 inventory，不物化全部历史 record。
5. **`SessionQuota`（`coda_tools::background`）** — 以 manifest capacity reservation
   计算 64 MiB session payload quota，并串行 task create、terminal eviction 和
   reservation release；持有 inventory blocker 时拒绝新 spawn。
6. **`BackgroundProcesses`（改）** — 保留进程与终态状态机；active task 使用打开的
   `TaskOutputFiles`，terminal task 可从 `TaskArchive` lazy-load。
7. **hub/storage wiring（`coda_server`）** — 为每个 `SessionKey` 提供打开后的 session
   archive directory capability，在 entry 初始化时恢复 archive，在 release 时持久化并
   关闭而不删除未读输出。
8. **`task_output` / TaskNotice（改）** — 分段返回新输出和 consumer-relative lost；
   完成通知报告 stream-level overwritten 总量。

## Interfaces

```rust
// coda_tools::background

/// 唯一可用于 archive 路径构造的 task id。
/// FromStr 只接受 `bg_` + 32 位小写 ASCII 十六进制；不 trim、不大小写归一化。
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct TaskId(String);

impl TaskId {
    /// 生成 UUID v4，并编码为严格的 canonical task id。
    pub fn new() -> Self;
    pub fn as_str(&self) -> &str;
}

impl std::str::FromStr for TaskId {
    type Err = InvalidTaskId;
    fn from_str(raw: &str) -> Result<Self, Self::Err>;
}

/// Custom Deserialize delegates to FromStr; derive(Deserialize) is forbidden because it could
/// construct an invalid path-bearing value from a manifest.
impl<'de> Deserialize<'de> for TaskId { /* validated implementation */ }

/// 已打开且限制在一个 session background 根目录内的 capability。
/// 所有后代操作都相对持有的 fd；接口不返回可供重新按路径打开的 PathBuf。
pub struct ArchiveDir { /* owned directory fd */ }

impl ArchiveDir {
    /// 惰性枚举直接子项；每次 next 只返回一个绑定到此目录 fd 的 name/type hint，
    /// 不跟随 symlink，也不把整个目录物化为 Vec。
    pub fn entries(&self) -> Result<ArchiveEntries<'_>, ArchiveError>;

    /// 以 O_NOFOLLOW 打开直接子目录，随后 fstat 验证 directory + owner/mode。
    pub fn open_dir(&self, name: &TaskId) -> Result<ArchiveDir, ArchiveError>;

    /// 在当前目录 fd 下创建 0600 regular file；O_CREAT|O_EXCL|O_NOFOLLOW，
    /// 返回后已对 fd 做 fstat/fchmod 验证。
    pub fn create_file(&self, name: &ArchiveFileName) -> Result<std::fs::File, ArchiveError>;

    /// manifest temp rename 与清理只能走同一目录 fd 上的 renameat/unlinkat。
    pub fn rename(&self, from: &ArchiveFileName, to: &ArchiveFileName)
        -> Result<(), ArchiveError>;
    pub fn unlink(&self, name: &ArchiveFileName) -> Result<(), ArchiveError>;
}

impl Iterator for ArchiveEntries<'_> {
    type Item = Result<ArchiveEntry, ArchiveError>;
}

pub struct OutputChunk {
    pub bytes: Vec<u8>,
    /// cursor 与当前 retained start 之间已经不可恢复的字节数。
    pub lost: u64,
    /// 仅推进到本次实际返回的末尾；仍有数据时下一次继续。
    pub next_cursor: u64,
    pub has_more: bool,
}

impl DiskTail {
    /// 追加整个字节片段。超过容量时覆盖最旧内容，同时保持逻辑偏移单调递增。
    /// owned transaction 持有 stream lock，调用方取消只放弃等待；flush/logical_range
    /// 会等待已经开始的 pwrite 与 offset commit 一起完成。
    pub async fn append(&self, bytes: &[u8]) -> std::io::Result<()>;

    /// 从绝对逻辑 cursor 起最多读取 limit 字节；若 cursor 落在 retained start
    /// 之前，返回仍可读的数据并在 lost 中精确报告被覆盖的未读字节。
    pub async fn read_from(
        &self,
        cursor: u64,
        limit: usize,
    ) -> std::io::Result<OutputChunk>;

    /// 返回当前保留内容的最后 limit 字节，不改变模型读取游标。
    pub async fn tail(&self, limit: usize) -> std::io::Result<Vec<u8>>;

    /// 确保数据和恢复所需的逻辑范围已写入文件；正常 release 的持久化屏障。
    pub async fn flush(&self) -> std::io::Result<()>;
}
```

```rust
pub struct TaskOutputManifest {
    pub manifest_version: u32,
    pub id: TaskId,
    pub command: String,
    pub description: String,
    pub agent_name: String,
    pub started_at: jiff::Timestamp,
    pub terminal_at: Option<jiff::Timestamp>,
    pub status: TaskStatus,
    pub stdout: StreamManifest,
    pub stderr: StreamManifest,
    pub output: OutputDisposition,
}

pub struct StreamManifest {
    /// 决定物理布局的格式版本；未知版本拒绝读取。
    pub layout_version: u32,
    /// 文件创建时的容量。reopen 必须使用此值，不能使用当前默认配置。
    pub capacity: u64,
    pub start_offset: u64,
    pub total_written: u64,
    pub read_cursor: u64,
    /// 已推进 byte cursor、但尚不足以组成完整 UTF-8 scalar 的 0..=3 bytes。
    pub utf8_carry: Vec<u8>,
}

pub enum OutputDisposition {
    Retained,
    Consumed { at: jiff::Timestamp },
    Expired { at: jiff::Timestamp, reason: ExpireReason },
}

pub enum ExpireReason {
    SessionQuota,
}

/// 新增的 terminal states；与 Exited/Killed 一样一旦提交不可再改变。
pub enum TaskStatus {
    // 现有 Running / Exited / Killed……
    Failed { message: String, at: jiff::Timestamp },
    Interrupted { at: jiff::Timestamp },
}

pub enum TaskExit {
    // 现有 Exited / Killed……
    Failed { message: String },
}

impl TaskArchive {
    /// 顺序枚举本 session 的全部 task 目录并分类、计费；每项检查后立即关闭 task/ring
    /// fd，不删除或修复。返回值只含有界轻量索引；单项 issue 不阻止返回。
    pub async fn inventory(&self) -> Result<ArchiveInventory, ArchiveError>;

    /// 创建 task 的私有输出目录和两个 stream 文件；全部成功后才允许启动进程。
    /// 必须消费 SessionQuota 发出的 reservation；失败时 reservation guard 自动回滚，
    /// 且不留下半成品目录。
    pub(crate) async fn create(
        &self,
        id: &TaskId,
        meta: &TaskMeta,
        reservation: QuotaReservation,
    )
        -> std::io::Result<(Arc<TaskRecord>, QuotaReservation)>;

    /// 按 id 打开 archived task（包括 crash 遗留的 Running）。未知、已清理或 manifest
    /// 损坏时返回错误，不把损坏记录误报成一个新的空 task。
    pub async fn open(&self, id: &TaskId)
        -> Result<Option<Arc<TaskRecord>>, ArchiveError>;

}

impl TaskRecord {
    /// read/terminal/release/reopen/eviction 修改该 task 前必须取得的线性化 guard。
    pub async fn lock_commit(&self) -> TaskCommitGuard<'_>;
}

impl TaskCommitGuard<'_> {
    /// 当前已提交状态的只读视图，用于构造满足单调不变量的 candidate。
    pub fn current(&self) -> &TaskPersistentState;

    /// 校验单调不变量，原子保存完整 manifest，成功后才替换内存状态。
    /// 调用期间 guard 始终持有；没有可绕过 guard 的 public archive save API。
    pub async fn commit(
        &mut self,
        candidate: TaskPersistentState,
    ) -> Result<(), ArchiveError>;
}

impl SessionQuota {
    /// 以完整 inventory 初始化 reservation/blocker；不执行删除或淘汰。
    pub fn from_inventory(inventory: &ArchiveInventory, limit: u64) -> Self;

    /// 为新 task 串行取得 reservation；必要时按 terminal_at 淘汰 terminal victim。
    /// victim 只有在 manifest→delete 完成后才 release；delete 失败会保留计费并进入
    /// residual retry。即使 reservation 失败，已提交的 expiration facts 仍随结果返回。
    /// 固定为当前 task output layout 的两个 stream capacity；调用方不能指定 bytes。
    /// 返回的一次性 QuotaReservation 不可 Clone。
    pub async fn reserve_for_create(&self) -> ReserveOutcome;

    /// cursor commit 后调用；重新按 quota→commit 锁序确认 terminal + fully consumed，
    /// 再执行 manifest→delete→release。Running 或未读完时是无操作。
    pub async fn finalize_consumed(&self, record: &TaskRecord) -> Result<(), ArchiveError>;

    /// terminal manifest 已提交且 commit lock 已释放后调用；在 quota→commit 锁序下
    /// recheck：fully consumed 则执行 Consumed 清理，否则把轻量 RetainedIndexEntry
    /// 注册为 quota victim。错误不能阻止 completion notice/summary 发布。
    pub async fn finalize_terminal(&self, record: &TaskRecord) -> Result<(), ArchiveError>;
}

pub struct ReserveOutcome {
    pub reservation: Result<QuotaReservation, QuotaError>,
    /// 本次尝试中已经 durable commit 的 expiration facts，与 reservation 成败无关。
    /// 函数返回时 quota lock 已释放；SessionQuota 从不访问 registry notice queue。
    pub expirations: Vec<ExpirationFact>,
}

pub struct ExpirationFact {
    pub id: TaskId,
    pub expired_at: jiff::Timestamp,
    pub reason: ExpireReason,
}

pub struct ArchiveInventory {
    /// 仅 Retained terminal 的 quota victim 元数据；无 TaskRecord、无打开 fd。
    /// 正常 inventory 中长度由 quota/min-capacity 证明不超过 512。
    pub retained: Vec<RetainedIndexEntry>,
    pub retained_count: u64,
    pub retained_index_truncated: bool,
    /// 按 terminal_at 最新的至多 32 条，用于 live overview 初始化。
    pub recent_terminal: Vec<TaskSummary>,
    pub reserved_bytes: u64,
    pub issue_count: u64,
    /// 至多 MAX_INVENTORY_ISSUE_SAMPLES = 32；只用于诊断/UI，不参与 correctness。
    pub sampled_issues: Vec<InventoryIssue>,
    /// issues、reserved > limit 或 retained_index_truncated 时为 true。
    pub spawn_blocked: bool,
}

pub struct RetainedIndexEntry {
    pub id: TaskId,
    pub terminal_at: jiff::Timestamp,
    pub stdout_capacity: u64,
    pub stderr_capacity: u64,
}

pub enum InventoryIssue {
    CorruptTask { id: Option<TaskId>, charged_bytes: u64, error: String },
    OrphanEntry { name: String, charged_bytes: u64 },
    UnsafeEntry { name: String, error: String },
}
```

`TaskArchive` 的任何 path helper 都只接受 `&TaskId`。`task_output`/`task_kill` 参数虽仍
是 JSON string，但工具入口必须先 `parse::<TaskId>()`；非法格式返回 unknown/invalid
task id，绝不能进入 `PathBuf::join`。manifest 反序列化出的 id 也必须再次通过同一格式
校验，并与父目录名完全相等。

reopen 一个 retained stream 时必须验证：

```text
layout_version 是受支持版本
64 KiB <= capacity <= 64 MiB
start_offset <= total_written
total_written - start_offset <= capacity
read_cursor <= total_written
utf8_carry.len() <= 3 且确为一个不完整 UTF-8 前缀
ring 通过父 dir fd + O_NOFOLLOW 打开，opened-fd fstat 为正确 owner/mode 的 regular file
ring file length == min(total_written, capacity)
Consumed/Expired => ring 可以不存在
Retained => ring 必须存在且长度一致
Running => terminal_at 为空且 output 为 Retained
terminal status => terminal_at 非空
Consumed => terminal status、两个 read_cursor == total_written、utf8_carry 为空
Expired => terminal status；read_cursor 可以落后 total_written
```

任何不一致都返回 `ArchiveError::Corrupt`，不能用当前默认 capacity 猜测修复。

### Session-local inventory

entry reopen 必须通过 `ArchiveDir` 枚举该 session 的 `background/tasks` 直接子目录；
这是恢复当前 session 的必要步骤，不是 server-global 启动清理。inventory 不 rename、
unlink 或修复任何条目，只分类和计费。scan 必须顺序/streaming：每个 task directory、
manifest 和 ring fd 在该项完成校验/计费后立即关闭，任意时刻只允许常数个 inventory fd。

| 条目 | reservation 计法 | 可读性 | 对 session 的影响 |
| --- | --- | --- | --- |
| 有效 `Retained` | stdout/stderr manifest capacity 之和 | 正常恢复 | 无 blocker |
| 有效 `Consumed/Expired` 且 ring 已不存在 | 0 | 返回 consumed/expired 语义 | 无 blocker |
| 有效 `Consumed/Expired` 但上次删除失败、ring 仍在 | 每个 safely-opened regular ring 的 `fstat.len()` | 正文仍按 disposition 禁止读取 | 无全局 blocker；下次 create 可重试删除，成功前继续计费 |
| manifest 损坏、未知版本、layout/file 不一致 | 对该 task dir 内可安全打开的 direct regular files 累加 `fstat.len()` | 该 task 返回 archive-corrupt | 增加 session spawn blocker |
| 缺少 meta、非法目录名或只有 ring 的 orphan | 对可安全打开的 direct regular files 累加 `fstat.len()` | 不恢复为 task | 增加 session spawn blocker |
| symlink、socket、device、nested directory 等 unsafe 项 | 已确认的 regular files 仍计长度；未知项不跟随、不猜大小 | 不读取 | 增加 session spawn blocker |

规则：

- `reserved_bytes` 可以大于 64 MiB；inventory 仍成功返回并允许 session attach，但任何
  新 background spawn 都返回明确的 `archive inventory blocked` 错误。
- Consumed/Expired 且无残留 ring 的记录在校验和 recent-summary 计算后立即丢弃，不创建
  `TaskRecord`，也不进入长期 index；按 id 查询时再 lazy open manifest。
- 校验通过的 crash-Running 为了原子转 Interrupted 可以短暂打开一个 `TaskRecord`，但
  必须在处理下一目录前释放所有强引用/fd；inventory 不把它放进返回值或 Weak live map。
- Retained terminal 只生成 `RetainedIndexEntry`。它不持有 task/ring fd、manifest 正文或
  commit lock，只包含 victim 选择所需的 id、terminal_at 和两个 capacities。
- supported stream capacity 下限为 64 KiB，因此正常的 64 MiB inventory 最多有
  `64 MiB / (2 × 64 KiB) = 512` 个 retained entries。vector 硬上限为 512；若实际
  retained_count 更多或 reserved 超 quota，继续 streaming 计总数/bytes，但停止追加
  index 并设置 spawn blocker，不能用一个异常 archive 制造无界 vector。
- `recent_terminal` 用固定大小 32 的 min-heap/等价算法 streaming 维护，最终按
  `terminal_at` 排序；不为其创建 `TaskRecord`。
- issue 只累加 `issue_count` 并保存前 32 个 `sampled_issues`；超过上限只计数。blocker
  由 `issue_count > 0` 决定，不能因为 sample 被截断而漏掉；每个 sample 的 name/error
  分别最多 512 bytes。
- `meta.json` 先以 opened-fd `fstat.len()` 检查 64 KiB 上限，超限不读取正文、直接按
  corrupt 处理；因此单项 manifest 也不能制造无界瞬时分配。
- 单个 corrupt/orphan 不阻止 session attach，不影响其他 valid task 的
  `task_output`/`task_kill`。对具有合法 TaskId 但损坏的记录，查询返回该 task 的
  archive-corrupt 错误；不能降级成 unknown。
- 只有 session background 根目录本身无法安全打开/枚举时，archive 初始化失败；hub
  attach 仍可恢复对话，但 background registry 进入 disabled 状态，既不恢复 task 也不
  允许 spawn，并把错误作为 server warning/工具错误暴露。不能让输出子系统阻断整个
  对话 session。
- blocker 本版没有自动修复 API；解除方式是用户删除 session 或在 server 停止后人工
  处理目录。entry/server 启动、attach 和 inventory 本身都不删除文件。
- quota 只对 Coda 管理的 ring payload 给出 64 MiB 上限。外部同 Unix 用户主动写入
  archive 仍可造成磁盘 DoS，但 fd-relative no-follow 操作保证不会因此让 Coda 越过
  session archive 访问其他路径。

该策略保证 corrupt/orphan 永远不会被静默少算后继续创建 task：能测量的 bytes 保守
计费，不能可信解释的项则通过 spawn blocker 封闭增长路径；同时 reopen 的常驻内存
只与 512 个 retained index、32 条 summary 和 32 条 issue sample 的固定上限相关，不与
Consumed/Expired 历史总数相关。inventory 时间仍与目录数线性增长，这是已知取舍。

### Archive path safety

`symlink_metadata(path)` 只能用于诊断，不能作为安全检查。安全 contract 是：

1. `WorkspaceStorage` 从其已信任的 workspace/session root directory fd 出发，逐层用
   `openat(O_DIRECTORY | O_NOFOLLOW)` 打开或创建 `background/tasks`，并把最终 owned fd
   包装成 `ArchiveDir`；
2. task directory 先 `mkdirat(mode=0700)`，再从 tasks dir fd 用
   `openat(O_DIRECTORY | O_NOFOLLOW)` 打开，并对返回 fd `fstat` 验证 owner、directory
   type 和 mode；
3. ring/manifest/temp file 只从 task dir fd 使用
   `openat(O_NOFOLLOW | O_CREAT/O_EXCL as appropriate)`，随后对返回 fd `fstat` 验证
   regular file、owner 和 `0600`；必要时 `fchmod` 后再次验证；
4. manifest commit 使用同一 task dir fd 上的 `renameat`，删除使用 `unlinkat`；从检查
   到操作不重新解析 ambient absolute/relative path；
5. inventory 对 directory entry name 先解析 canonical `TaskId`，再以父 dir fd 打开；
   无论枚举后名称如何被替换，`O_NOFOLLOW` + opened-fd `fstat` 都不会跟随到 archive
   外部。

Consumed/Expired 清理只通过已打开 task dir fd unlink 已知 ring files，并保留
`meta.json`/task directory；不在并发可篡改的父目录里按名称递归删除 task dir。create
回滚若无法证明父目录项仍是刚创建的 inode，宁可留下可被下次 inventory 阻塞的 orphan，
也不删除一个可能已被替换的目录。

实现可以用 `rustix`/等价的安全封装，但不能退化为 path metadata→path open。所有 fd
在 blocking filesystem worker 上操作，异步层只持有 capability object，不暴露裸 fd
所有权或可重新 join 的路径。

```rust
impl BackgroundProcesses {
    /// hub-owned registry：调用方提供已安全打开的 session archive capability；entry
    /// reopen 时先 inventory 再恢复。registry shutdown 只 flush/close，不删除未读输出。
    pub fn session_backed(archive_dir: ArchiveDir) -> Self;

    /// session archive 根 capability 无法安全打开时使用：对话 session 继续工作，
    /// summaries 为空，spawn/read/kill 返回同一明确 archive-disabled 错误。
    pub fn disabled(error: ArchiveError) -> Self;

    /// 独立 Session 使用：输出位于 registry-owned 临时目录；Session 的 runtime
    /// 已确认退出且 registry shutdown 后没有后续消费者，因此可删除整个目录。
    pub fn temporary() -> Self;

    /// 增量读取 active 或 archived task。每个 stream 最多返回 READ_CHUNK_LIMIT；
    /// manifest/cursor 写失败返回 Err，不能返回内容后假装游标已可靠推进。
    pub async fn read(&self, id: &TaskId) -> Result<Option<TaskRead>, TaskReadError>;

    /// active task 执行 kill；archived terminal 返回既有状态。corrupt/disabled archive
    /// 返回明确错误，未知 canonical id 才返回 Ok(None)。
    pub async fn kill(&self, id: &TaskId)
        -> Result<Option<TaskStatus>, TaskControlError>;
}
```

`TaskRead` 继续包含 `stdout_lost` / `stderr_lost`。`task_output` 的模型文案保持
明确，例如：

```text
status: exited with code 0
(32768 bytes of stdout were overwritten before they could be read)
stdout (new; more remains):
...
```

完成通知增加 storage-relative 的覆盖事实，而不是把它误称为当前 consumer loss：

```rust
TaskNotice::Task {
    // 现有字段……
    output_tail: String,
    stdout_overwritten: u64,
    stderr_overwritten: u64,
}

/// Quota eviction may happen after the completion notice was already delivered,
/// so expiration is a new fact with its own notice rather than a mutation of old history.
TaskNotice::OutputExpired {
    id: TaskId,
    expired_at: jiff::Timestamp,
    reason: ExpireReason,
}

/// coda_core::llm 中的 wire/history 类型；只用于身份，不用于构造 archive path，
/// 因此用 String 避免 coda_core → coda_tools 反向依赖。status、timestamp、reason
/// 是 payload，不属于 identity；同一事实重试时 key 必须完全稳定。
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum TaskNoticeKey {
    Completed { task_id: String },
    OutputExpired { task_id: String },
    /// 每个 overflow aggregate 创建时生成并随 NoticeStore 持久化的稳定身份。
    OverflowBatch { batch_id: String },
}

/// 现有 overflow 从 (id, status) 扩展为可表达 completion/expiration 两类事实，
/// 保持“输出细节可降级，terminal/expiration 事实不可静默丢弃”的原则。
pub enum TaskNoticeFact {
    Completed { id: TaskId, status: TaskStatus },
    OutputExpired { id: TaskId, reason: ExpireReason },
}

TaskNotice::Overflow {
    /// 创建 aggregate 时生成一次的 UUID canonical string；merge/NoticeStore roundtrip 保持。
    batch_id: String,
    dropped: Vec<TaskNoticeFact>,
    uncounted: u64,
}

impl TaskNotice {
    /// 返回本 notice 覆盖的全部稳定事实键；Completed 与 OutputExpired 即使 task id
    /// 相同也产生不同 key。
    pub fn keys(&self) -> Vec<TaskNoticeKey>;
}
```

- `*_overwritten = start_offset`：该 stream 因文件容量限制覆盖过多少历史字节，
  与模型是否已在覆盖前读过无关；适合稳定写入完成通知。
- `TaskRead::*_lost = start_offset.saturating_sub(read_cursor)`：当前 consumer 尚未
  读取便丢失多少字节；读取时计算并只报告一次。
- `TaskNotice::OutputExpired`：无论原 completion notice 是否已 take/checkpoint，都
  通过现有通知队列在下一用户 turn 告知模型；它以
  `TaskNoticeKey::OutputExpired { task_id }` 参与去重和 overflow，绝不复用 completion 的
  `Completed { task_id }`。overflow 也必须保留“发生过 expiration”这一事实。`task_output` 对 expired
  task 返回 terminal status 加明确的 quota-expired
  说明，不返回 unknown，也不假装是 `(no new output)`。

wire/history 同步升级为：

```rust
pub enum UserOrigin {
    Human,
    TaskNotice { notice_keys: Vec<TaskNoticeKey> },
}
```

driver 注入通知历史时写入 `notice.keys()`；hub restore 从 checkpoint 收集
`HashSet<TaskNoticeKey>`，按 fact key 过滤 NoticeStore：

- `Completed { task_id: bg_x }` 只去重 completion；
- `OutputExpired { task_id: bg_x }` 只去重 expiration；
- overflow aggregate 先以持久化 `OverflowBatch { batch_id }` 判断整批是否已投递；
  未命中时再按逐项 fact key 过滤与其他 notice 重复的 details。`uncounted` 随 batch key
  去重，不需要为无法逐项表达的事实伪造 task id；
- status、时间和 reason 的变化不产生新身份，也不能让同一事实绕过去重。

## Data Model

```text
.coda/sessions/<session-id>/background/
 └─ tasks/
     └─ bg_<uuid>/
         ├─ meta.json       # 原子改写的小型 manifest，不含输出正文
         ├─ stdout.ring     # 固定容量
         └─ stderr.ring     # 固定容量
```

```text
BackgroundProcesses (owner = hub SessionEntry 或独立 Session)
 ├─ RegistryState
 │   ├─ running task entries
 │   ├─ 最近 terminal summaries（UI/keepalive 内存视图，≤32）
 │   ├─ notices / overflow
 │   └─ TaskArchive handle
 ├─ SessionQuota
 │   ├─ limit: 64 MiB
 │   ├─ reserved: valid Retained capacity + charged residual/corrupt file lengths
 │   ├─ retained_index: Vec<RetainedIndexEntry>  # ≤512, no fd/TaskRecord
 │   ├─ issue_count / sampled_issues             # samples ≤32
 │   ├─ spawn_blocked: bool
 │   └─ lock: serializes create / expire / reservation release
 └─ TaskEntry
     ├─ status / cancellation / metadata
     ├─ commit: Mutex<TaskPersistentState>
     │   ├─ status
     │   ├─ read_cursor: (stdout, stderr)
     │   └─ output disposition
     └─ TaskOutputFiles
         ├─ DiskTail(stdout)
         └─ DiskTail(stderr)

DiskTail（每 stream 独立锁）
 ├─ file handle
 ├─ layout_version
 ├─ capacity
 ├─ start_offset      # 当前仍保留的最早逻辑位置
 └─ total_written     # 下一个逻辑写入位置
```

环形文件的物理位置由 `logical_offset % capacity` 得到。一次 append/read 可能跨文件
末尾，必须拆成两段 I/O；逻辑偏移始终单调递增，不能把物理 wrap 次数暴露给调用方。

共享可变状态边界：

- stdout 与 stderr 各有独立锁，两个 pump 不相互阻塞；
- 同一 stream 的 append/read 通过 stream lock 串行，保证 reader 看到一个完整的逻辑
  范围；
- task status/cursor 与文件内容不共用 registry 全局锁；磁盘 I/O 期间不得持有
  `RegistryState` 锁；
- 每个 task 的 **commit lock** 串行 `task_output`、terminal commit、release、reopen
  转换和 quota eviction。一次提交必须在同一把锁内覆盖：

  ```text
  读取当前 cursor/status/disposition
    → 读取或 flush ring
    → 构造 candidate manifest
    → 写临时 manifest
    → atomic rename
    → 更新内存 TaskPersistentState
  ```

- manifest 提交不变量：`Running` 只能单调进入一个 terminal status；terminal status
  不可被另一终态或 Running 覆盖；cursor 只能前进且不得超过 total；两个并发 read
  必须返回不重叠区间。candidate 违反不变量时是内部错误，不执行 rename。
- `TaskArchive` 在一个短持有的 index mutex 下维护 `TaskId → Weak<TaskRecord>`；create/
  open 原子 get-or-insert，确保并发 lazy open 不会为同一 id 造出两把 commit lock。
  inventory 不向该 map 插入 record；只有 active create 或显式 `open(id)` 才 lazy-load。
  Weak 无 live owner 时才可移除，因此不会把 terminal 正文重新拉回常驻内存。
- quota lock 只负责 session reservation 和 victim 选择。全局锁序为：

  ```text
  SessionQuota lock → per-task commit lock → stdout/stderr stream lock
  ```

  普通 read 只取 commit → stream，不反向获取 quota lock；若 read 后达到 fully consumed，
  先完成 cursor commit 并释放 commit lock，再进入 quota cleanup，按全局锁序重新获取并
  recheck disposition/cursor。
- append 的 owned transaction 只持对应 stream lock，不获取 task commit 或 quota lock；
  pwrite 成功后在释放锁前提交 offsets。pump 等待方被取消时 transaction 仍异步完成，
  terminal flush/snapshot 获取同一 stream lock，因此必然越过所有已开始的 append。
- 任何 terminal/release manifest 若引用新的 `start_offset/total_written`，必须经过两个
  有序屏障：先 `stdout.flush` + `stderr.flush` 成功，再 snapshot 逻辑范围并 atomic
  save manifest。manifest 不得先于其引用的 ring bytes 落盘。`task_output` 只提交已经
  从 ring 成功读出的 cursor，同样遵守 manifest rename 成功后才更新内存和返回模型。

### Manifest 提交失败

- Running `task_output` 保存 cursor 失败：返回 tool error，不向模型返回已读 bytes，
  内存 cursor 保持原值；后续调用允许安全重读。
- terminal ring flush 或 terminal manifest save 失败：任务不能报告正常 Exited/Killed。
  monitor 在内存提交唯一的 `Failed { message }`，错误中保留原 process outcome 和 I/O
  context；completion notice 同样报告 Failed。release 在 commit lock 下 best-effort
  重试保存该 Failed manifest 一次。
- 重试仍失败：terminal summary/notice 仍正常发布，避免 keepalive 永久卡住；完整输出
  archive 视为不可恢复，不删任何不确定文件。pending notice 仍可由独立 NoticeStore
  持久化。内存 state 标记为 dirty，禁止继续 spawn，并在 shutdown/release 屏障内再次
  保存；该降级保持运行时可关停，但不声称重试成功前 task archive 已可靠保存。
- manifest blocking save 一旦开始，调用方取消只能取消等待，不能把 rename 和内存 state
  swap 拆开；owned commit transaction 持有 commit guard，异步完成 save + state swap，
  archive activity barrier 让 shutdown 等待所有 detached commit 收敛。
- task create 由 owned transaction 完成，并以 delivery ack 确认 record 已交给调用方；
  等待方取消或消失时，事务持有共享 quota lease 直到清理完成；清理失败则保留计费、设置
  spawn blocker 并记录错误。archive activity barrier 同样覆盖 detached create/cleanup。

### 生命周期

```text
spawn
  解析/生成强类型 TaskId
  quota lock 下检查 inventory blocker 并确保 1 MiB reservation
  必要时淘汰 terminal，active 永不淘汰
  reserve_for_create 返回并释放 quota lock
  registry 把 outcome.expirations 逐条入队为 OutputExpired notices
  创建 0700 task dir + 0600 stdout/stderr.ring + Running manifest
  全部成功（失败则回滚 reservation 与半成品）
  启动 GroupedChild 与 pipe pumps

running
  pipe EOF 语义不变
  pump append 到 DiskTail
  task_output 在 per-task commit lock 下分段读取并持久化 cursor

spool/read pump failure
  第一个失败成为唯一 failure cause（包含 stdout/stderr + read/append + I/O error）
  kill process group
  有界等待/停止另一个 pump
  reap leader
  进入 Failed terminal commit

terminal commit
  per-task commit lock 下：
    flush 两个 stream
    计算并保存 completion notice 所需的 output_tail / overwritten
    写 terminal manifest
  释放 commit lock
  调用 SessionQuota::finalize_terminal(record)：
    quota lock → commit lock → recheck terminal/disposition/cursors
    fully consumed（含零输出）→ Consumed manifest → delete rings → release reservation
    尚未 fully consumed → 注册无 fd 的 RetainedIndexEntry
  无论 finalize_terminal 成功或失败：
    用删除前已保存的 tail/overwritten 入队 completion notice
    最后发布 terminal summary
  cleanup error 只记录 warning/留待后续重试，不得跳过 notice/summary

`finalize_terminal` 的错误归类必须保持 quota 可追踪：若 Consumed manifest 尚未提交，
record 仍为 Retained 并必须进入 victim index；若 manifest 已是 Consumed 但 delete 失败，
reservation 保留并由后续 create 的 residual-cleanup 分支重试。两种失败都不能留下既不在
index、也不以 residual bytes 计费的空档。

entry release / 正常 server shutdown
  kill + join 所有仍运行 task（现有屏障）
  flush/close
  保存 manifest 与 pending notices
  不删除未读 session-backed 输出

entry reopen
  load pending notices
  fd-relative enumerate + inventory 全部 task archive entries（不删除）
  valid Retained 按 capacity、残留/corrupt regular files 按 length 重建 reservation
  有 issue 时设置 spawn blocker；attach 和 valid task 查询继续可用
  仅当遗留 Running manifest/rings 通过完整恢复校验时：
    per-task commit lock 下转为 Interrupted 并立即 atomic save
  校验失败的 Running 按 corrupt task 计费/阻塞，不转换
  valid terminal/interrupted task 可继续 task_output/task_kill

terminal output fully consumed
  stdout_cursor == stdout_total_written
  && stderr_cursor == stderr_total_written
  quota lock → commit lock，重新确认仍为 Retained 且 fully consumed
  先 atomic save OutputDisposition::Consumed
  再删除两个 ring 文件
  删除成功后释放 capacity reservation
  从 retained victim index 移除该 id（若存在）
  meta.json 保留 terminal status + Consumed

session quota eviction（仅在 create 需要 reservation 时触发）
  active task 永不进入候选
  Retained terminal 按 terminal_at 最老优先
  quota lock → victim commit lock，重新确认仍可淘汰
  若 terminal + fully consumed：
    强制走 Consumed manifest → delete → release；不生成 expiration fact
  否则：
    atomic save OutputDisposition::Expired(SessionQuota)
    删除两个 ring 文件
    删除成功后释放 capacity reservation
    从 retained victim index 移除该 id
    把 ExpirationFact 放入 ReserveOutcome（不操作 notice queue）

reserve_for_create 返回、quota lock 已释放
  registry 为每个 ExpirationFact 入队 TaskNotice::OutputExpired
  每条使用 OutputExpired task fact key（旧 Completed key 不匹配）

delete_session
  递归删除 background/ 在内的整个 session 目录
```

`TaskNotice` 已经 take、注入模型、写入 checkpoint 或在 UI 展示，都不构成输出已消费；
只有两个 stream 的读取游标都到达 terminal total 才构成消费完成。

fully consumed 的 terminal task 具有高于 quota expiration 的状态优先级：任何持有
quota lock 的路径在写 Expired 前都必须在 victim commit lock 下 recheck；一旦满足
terminal + cursors==totals，只能提交 Consumed。由此 terminal 后置 cleanup 与并发 create
无论谁先取得 quota lock，结果都相同，不会为已经读完或零输出 task 产生错误的
OutputExpired notice。

Consumed/Expired 清理都必须遵守“先 manifest、后删除、最后释放 reservation”。崩溃
发生在 manifest rename 后、文件删除前，只会造成安全的磁盘泄漏；manifest 不会声称
文件可读。文件删除失败时 reservation 不释放，新 task spawn 返回明确的 quota cleanup
错误；后续 create 可重试删除该已标记记录，但 entry/server 启动本身不主动清理。

### Pump failure 状态机

```rust
enum StreamName {
    Stdout,
    Stderr,
}

enum PumpResult {
    Eof,
    ReadFailed { stream: StreamName, source: std::io::Error },
    SpoolFailed { stream: StreamName, source: std::io::Error },
}
```

`run_process` 不再让 pump 返回 `()`，而是在一个 loop 中同时观察：

```text
cancellation
child exit
stdout pump result
stderr pump result
```

- `PumpResult::Eof` 只标记该 stream 已关闭；另一个 stream 或 child 仍活时不能提交
  terminal。
- 任一 `ReadFailed`/`SpoolFailed` 是 terminal failure，即使 leader 已先自然退出；主
  状态机保存第一个结构化错误、kill process group、reap leader，并用现有
  `PIPE_DRAIN_TIMEOUT` 有界等待另一个 pump。超时则 abort 另一个 pump。
- cancellation 和 pump failure 同时 ready 时使用 biased cancellation-first：用户已经
  发出的 kill 保持 `Killed`；否则第一个被状态机观察到的 spool/read failure 决定
  `Failed`。所有路径最终只把一个 `TaskExit` 交给 monitor。
- child 自然退出后仍继续等待 pipe EOF；这期间出现 pump failure 必须把候选 Exited
  升级为 Failed，不能返回 leader 的 exit code。
- failure stream 已无法继续保存，不尝试把其剩余 pipe 内容写进同一坏 sink；另一个
  stream 只在 group kill 后做有界 drain，保住已经可安全落盘的尾部。
- terminal manifest 保存失败不回到 `run_process` 重新决定 process outcome；它由上述
  terminal commit 降级规则统一转成带原 outcome 的持久化 Failed，并保证 monitor
  仍然完成唯一终态发布。

## Load-Bearing Decisions

1. **保留 pipe，文件只替换 pump 的 sink。** 直接重定向会失去后台后代持有
   stdout/stderr 时的 EOF 生命周期信号，并重新引入 leader 已退出却误判任务完成的
   问题。代价是每个 stream 仍有一个 8 KiB pump buffer 和磁盘写入背压。

2. **选择固定容量环形文件，不选择无限 append。** 环形文件保持当前尾部语义和严格
   磁盘上界；覆盖造成的信息损失通过绝对偏移显式报告。代价是实现和测试复杂度高于
   普通文件，而且文件本身不是按物理顺序可直接阅读的日志。每个 stream 的
   `layout_version` 和创建时 `capacity` 属于持久化格式；配置变化只影响新文件。

3. **输出 owner = session，而不是 hub entry。** detached task 完成会触发 entry
   release；若文件跟 entry 一起清理，模型下一 turn 只能看到 4 KiB notice tail。
   session-owned archive 让输出跨 release/reopen 和正常重启可读，代价是需要独立
   manifest 和 lazy reopen。

4. **完整输出不进入 checkpoint。** checkpoint 只恢复 agent/runtime 状态；输出正文
   只存在 ring 文件，小型 task 元数据只存在独立 manifest。这样避免 checkpoint
   膨胀、重复整篇改写和 runtime/spool 双写者。manifest 是 task archive 的索引，
   不是 runtime checkpoint 的字段。

5. **内存 terminal 上限与磁盘可读期限解耦。** 最近 32 条 summary 只是 live UI
   视图；从内存 map 淘汰不能删除未读文件。`task_output`/`task_kill` 在内存 miss 后
   查询 archive，以 task id 保持可达。

6. **notice delivery 不等于 output consumption。** 通知的作用是让模型知道应当读取；
   notice take、checkpoint 或 UI 展示均不推进 cursor，也不触发清理。

7. **两类丢失指标分开。** notice 使用稳定的 `overwritten`（存储事实），read 使用
   cursor-relative `lost`（消费者事实），避免模型已经读过早期内容后通知仍声称其
   “未读丢失”。

8. **读取必须分段。** 输出移到磁盘并不意味着可以把任意大小内容一次塞回模型；
   每 stream 每次最多 128 KiB，减少瞬时 `Vec/String` 和 context 占用。persistent
   memory 中没有输出正文，但一次 tool response 仍必然临时分配返回内容。

9. **磁盘错误是任务失败，不静默吞输出。** 创建输出文件失败时不启动进程；运行中
   append 失败时终止进程组，并提交明确的 `Failed { message }` 终态。read/manifest
   失败作为 tool error 返回。为此需扩展 `TaskExit`/`TaskStatus`，不能误报 `Killed`
   或正常 exit。pump 必须返回结构化结果并由 `run_process` 同时监听，不能把写盘失败
   当作普通 EOF。

10. **本版不做启动清理。** 正常消费、session 删除和正常 owned shutdown 仍清理；
    crash/kill -9 可能留下孤儿目录，下一次启动不删除。该风险明确接受，未来可单独增加
    带活跃实例保护的 stale cleanup，不能在本实现中顺手加入。entry reopen 必须枚举本
    session archive 做 inventory/计费；这是恢复，不是清理。quota 超限也只在新 task
    create 时触发 eviction，不在启动时触发。

11. **hub-owned 与 owned Session 使用不同保留策略。** hub-owned 使用 session-backed
    archive；独立 owned Session shutdown 后不存在未来消费者，可使用 registry-owned
    临时目录并在确认 runtime 退出后删除。两者共用 `DiskTail`，只改变 output root 和
    shutdown policy。

12. **per-task commit lock 是持久化线性化点。** “registry 唯一写者”不足以阻止其内部
    多条 async 路径乱序 rename。read、terminal、release、reopen conversion、quota
    eviction 必须在同一 task lock 下完成 read-state→I/O→rename→memory-commit；由此保证
    status 单调、cursor 单调和并发读取不重叠。代价是同一 task 的 read/terminal 会
    互相等待，但不同 task 仍完全并发。

13. **archive path 只能由强类型 `TaskId` 构造。** 模型输入在 tool boundary 严格解析为
    `bg_` + 32 位小写十六进制；archive API 不接受裸字符串。这把路径穿越防护放在类型
    边界，而不是依赖每个 join call 记得校验。

14. **session quota = 64 MiB capacity reservation。** create 和 eviction 由 quota lock
    串行；active 永不淘汰，terminal 按 `terminal_at` 最老优先。淘汰先保存 Expired、
    再删 ring、最后释放 reservation，并总是产生独立 OutputExpired notice。选择按
    capacity 而非当前 file length 计费，接受利用率较保守，换取 active task 不会在增长
    过程中突破 quota。reopen 对 valid Retained 按 manifest capacity 计费，对
    Consumed/Expired 残留和 corrupt/orphan regular files 按 file length 计费；未知项设置
    spawn blocker。即使 reserved 已超限也允许 attach，但不允许新 spawn。

15. **清理是 manifest-first 的单向事务。** Consumed/Expired 必须先原子保存不可读
    disposition，再删除文件；反向顺序会留下“manifest 声称可读、文件已不存在”的危险
    状态。删除失败保留 reservation，因此最多拒绝新 spawn，不会悄悄超配磁盘。

16. **UTF-8 不改变 byte cursor。** `read_cursor` 永远按原始 bytes 单调推进；展示层把
    chunk 末尾最多 3 个不完整 UTF-8 bytes 放进持久化 `utf8_carry`，下次与新 bytes
    拼接解码。terminal EOF 时剩余 carry 以 replacement character 收尾；发生 consumer
    loss 时先收尾旧 carry，再报告 lost。不能为了字符边界把持久化 cursor 回退。

17. **权限、文件类型和 no-follow 是 archive contract。** 创建目录显式设为 `0700`、
    文件 `0600`；所有后代操作相对已打开 directory fd，open 使用 `O_NOFOLLOW`，然后
    `fstat` 已打开 fd；manifest rename/delete 使用 `renameat`/`unlinkat`。单独的
    `symlink_metadata` 只能诊断，不能作为授权检查。代价是需要 Unix fd-level 文件模块，
    但消除了 check/open TOCTOU 和父路径替换。

18. **通知按 fact identity 去重，不按 task id 去重。** `Completed { task_id }` 与
    `OutputExpired { task_id }` 是同一 task 的两个独立事实；`UserOrigin`、notice keys、
    overflow 和 restore dedupe 全部使用 `TaskNoticeKey`。status/time/reason 不进入 key，
    保证重试身份稳定。每个 overflow aggregate 另有持久化 batch id，覆盖无法逐项表达
    的 `uncounted` 身份。

19. **quota 不操作 notice queue。** `reserve_for_create` 的 owned transaction 在 quota
    lock 内完成淘汰和 reservation；durable expiration 先进入 quota 自身的 pending-fact
    队列，正常返回时随 `ReserveOutcome` 交付，等待方取消时留待 registry 的 notice drain/
    shutdown 提取。这样锁图保持 quota→commit→stream，不新增 quota→registry 的反向依赖；
    即使调用方取消或后续新 task 文件创建失败，已经发生的 expiration 仍会通知。

20. **corrupt 是局部读取失败、session 级 spawn blocker。** inventory 对能测量的文件
    保守计费，任何 corrupt/orphan/unsafe 项都禁止新 background spawn，但不阻止 attach
    或其他 valid task 查询。选择可用性优先于“一个坏 task 关闭整个 session”，同时通过
    blocker 保证损坏文件不能绕过 quota 后继续增长。

21. **消费清理由 cursor 和 terminal 两个事件共同触发。** Running read 追平当时 total
    不能清理；terminal manifest 提交后必须调用 `finalize_terminal` 再次 recheck。quota
    eviction 的最终 victim recheck 把 fully consumed 强制归为 Consumed，因此与并发
    create 的先后顺序无关。tail/overwritten 在任何删除前保存，cleanup 失败仅告警，
    completion notice/summary 必须照常发布。

22. **inventory 是 streaming/bounded index，不是 task restore。** reopen 顺序扫描每项并
    立即关闭 fd；Consumed/Expired 历史不物化 record。内存只保留至多 512 条无 fd
    retained victim index、32 条 recent summary、32 条 issue sample 和总计数。完整
    `TaskRecord` 仅由 active create 或显式按 id 查询 lazy-open，Weak map 不被 inventory
    填充。接受 reopen 时间与历史目录数线性增长，拒绝内存/FD 也线性增长。

## Risks / Open Questions

1. **最大风险：环形文件的逻辑/物理映射。** 大片段超过 capacity、恰好落在边界、
   多次 wrap、cursor 落在 start 前后和 UTF-8 跨 chunk 都容易产生 off-by-one。第一步
   用纯 `DiskTail` 测试锁死，不先接进程。
2. **manifest 与 ring 的崩溃一致性。** 正常 release 有 flush + atomic manifest
   barrier；异常退出可能出现数据已写而 offset 尚未持久化，或相反。本版只承诺
   best-effort 恢复，不以 fsync/事务把它升级为 crash-safe 日志。
3. **Running manifest 的重启语义。** 正常 shutdown 会先 kill/join 并写 terminal；
   crash 后遗留的 Running 记录不能恢复进程。只有 manifest 与 ring length/layout 仍
   通过完整恢复校验时，才在返回 reopen 前原子转存为 `Interrupted`；常见的“ring 已
   追加、manifest total 尚未更新”会按 corrupt task 计费并设置 spawn blocker，而不是
   承诺可读。孤儿进程本身仍属原设计接受的 out-of-scope 风险。
4. **quota eviction 的失败组合。** victim manifest 已标 Expired 但文件删除失败时，
   reservation 必须保留，可能导致后续 spawn 被拒绝。实现要让下一次 create 重试这类
   已标记 victim 的删除，并测试不会重复发送 OutputExpired 或重复释放 reservation。
5. **何时删除已消费文件。** 本文选择双 cursor 到 terminal total 后立即删除 ring、
   保留 terminal meta。若产品需要重复下载完整日志，这一选择必须在实现前改为 TTL；
   当前 `task_output` 本就是单消费者增量接口，没有重复读取契约。
6. **I/O 背压。** 8 KiB pump 每次落盘会比内存 append 慢，chatty process 可能因 pipe
   反压而降速。这是安全退化，但需用连续输出测试确认不会阻塞 registry 锁或 hub。
7. **manifest 写频率。** 每次 `task_output` 都要可靠保存 cursor，否则 release/reopen
   后可能重复返回内容。可以接受小型 JSON 原子写的成本；若实测过高，再把 cursor
   单独放入固定大小 header，不提前设计双格式。
8. **UTF-8 carry 与 lost 的组合。** carry 最多 3 bytes，但 crash/reopen、terminal EOF
   和 cursor 落后 start 时必须确定性收尾，不能重复 replacement 或吞掉下一 scalar；
   用跨每个 byte boundary 的多语言 property test 验证，byte cursor 永不回退。
9. **锁序实现偏差。** quota cleanup、read-after-consume 和 terminal commit 若出现
   commit→quota 的反向获取会死锁。测试需用 barrier 人工交错 create/read/terminal/
   eviction，并在 code comment 中把 quota→commit→stream 标为硬性协议。
10. **inventory 的保守计费不是磁盘全局审计。** regular file 使用逻辑 length 计费，
    unsafe 项通过 blocker 封闭增长；同 Unix 用户仍可在 Coda 之外制造磁盘 DoS。本设计
    保证 Coda 不跟随路径逃逸且不在已知损坏下继续 spawn，不承诺防御账号所有者本身。
11. **历史 metadata 仍会增加 reopen 时间。** streaming inventory 把内存/FD 降为有界，
    但必须检查每个 task directory，耗时仍是 O(history)。若实测 session 历史过大，再
    设计 compaction/index checkpoint；本版不为性能引入第二个权威索引。
12. **terminal cleanup 与 quota create 的交错。** 两条路径都走 quota→commit，最终
    recheck 必须共享同一 helper；若各写一份判断，未来容易让 fully consumed 被 Expired。
    用 barrier 测试两种抢锁顺序，并把“Consumed 优先于 Expired”作为状态机不变量。

## Implementation Roadmap

> 进度（2026-07-11）：**全部 10 个 phase 已实现并通过验证**——磁盘存储引擎、archive、
> manifest 事务、inventory + quota、registry/process/hub/notice 集成、session-backed
> 持久化、UTF-8 carry、web 状态渲染。`cargo clippy --workspace --all-targets` 干净、
> `cargo test --workspace` 全绿、`pnpm --filter coda-web lint` + `tsc` 干净。详见每项
> 末尾的落地说明与文末《实现说明》。

- [x] **[risk validation] 实现独立 `DiskTail`，不接 registry** — 已实现
      Purpose：先证明固定文件容量下的逻辑偏移、wrap、lost 和 tail 正确。
      Verification：覆盖空文件、单次超容量、恰好 capacity、多轮 wrap、跨边界读、
      cursor 落后、分段 read/has_more、不同 persisted capacity reopen 的单元测试；
      property test 与等价内存模型逐操作比较；未知 layout 和 file length/offset 不一致
      一律报 corrupt。
      落地：`crates/coda_tools/src/background/disk_tail.rs`（`background.rs` 已改为
      目录模块 `background/mod.rs`）。`DiskTail` 以 `Arc<std::fs::File>` + positioned
      `pread`/`pwrite` 在 blocking pool 上做环形读写，per-stream `tokio::Mutex` 串行
      append/read；`OutputChunk` 报告 `lost`/`next_cursor`/`has_more`；`create`/`reopen`
      校验 capacity 上下界、`start_offset`/`total_written` 一致性与 ring 文件长度，
      不一致返回 `InvalidData`（后续 phase 映射为 `ArchiveError::Corrupt`）。测试含
      14 个用例，其中 `differential_against_model` 以确定性 xorshift 在 cap∈{1,2,3,7,8,16,33}
      上对拍逐操作参考模型。`LAYOUT_VERSION` 常量留待 phase 2 manifest 消费。
      验证命令：`cargo clippy -p coda_tools`、`cargo test -p coda_tools` 全绿。

- [x] **[storage model] 实现 task 目录和原子 manifest** — 已实现
      Purpose：建立 session-owned archive 与 terminal lazy reopen 边界。
      Verification：`TaskId` 拒绝 `../`、绝对路径、大小写和非 canonical 长度；所有 path
      helper 在类型上不接受 `&str`；create/open/guarded-commit/remove roundtrip；
      directory fd + openat(O_NOFOLLOW) + opened-fd fstat；用 barrier 在枚举/检查后替换
      symlink 仍不能逃出 archive；0700/0600、non-regular 拒绝；损坏 manifest 返回明确
      错误；Consumed/Expired 后 ring 删除但 status 仍可查询；session 目录递归删除全清理。
      落地：新增 `task_id.rs`（强类型 `TaskId`，`FromStr`/`Deserialize` 严格校验，
      拒绝 `../`/绝对路径/大小写/非 canonical），`archive_dir.rs`（`ArchiveDir` 以
      `libc` `openat`/`mkdirat`/`renameat`/`unlinkat` + `O_NOFOLLOW` + opened-fd
      `fstat` 做 fd-relative 操作，`ArchiveFileName` 闭集限定文件名，`entries` 惰性
      迭代不物化目录），`manifest.rs`（`TaskOutputManifest`/`StreamManifest`/
      `OutputDisposition`/`ExpireReason` + reopen 结构校验含 UTF-8 carry prefix 判定），
      `task_archive.rs`（`TaskArchive` weak-index get-or-insert、`TaskRecord` +
      per-task commit lock、`TaskCommitGuard::{current,commit,delete_rings}` 强制
      status/cursor/disposition 单调 + manifest-first 原子保存、`TaskOutputFiles`、
      terminal lazy reopen、rings 删除后经 `DiskTail::detached` 仍可查 status）。
      `TaskStatus` 新增 `Failed`/`Interrupted`，`TaskExit` 新增 `Failed`。测试覆盖
      symlink 拒绝、非目录拒绝、重复 create 拒绝、terminal 不可变、cursor 不回退、
      Consumed 转换删除 ring 后重开仍可查询、manifest roundtrip 与校验拒绝。

- [x] **[inventory] 实现 session-local archive inventory** — 已实现（含 quota）
      Purpose：在恢复任何 task 前建立完整 reservation 和 corruption blocker，不能让坏
      文件绕过 quota。
      Verification：valid Retained 按 persisted capacity；Consumed/Expired 残留按 file
      length；missing/corrupt/unknown-layout/orphan/unsafe 项均不静默忽略；损坏目录存在时
      quota 不少算且新 spawn 被拒绝；session attach 和其他 valid task 查询仍成功；根目录
      无法安全打开时 background disabled 但对话仍可 attach；inventory 不删除任何文件；
      构造大量 Consumed/Expired 和 orphan 后断言 fd 峰值为常数、recent summaries≤32、
      issue samples≤32、retained index≤512，且 Weak record map 未被 inventory 填充；
      directory entries 为 lazy iterator，超 64 KiB manifest 不读取正文，sample 文本有界。
      落地：新增 `quota.rs`。`scan_inventory` streaming 分类每个 task 目录：valid
      Retained 按 capacity 计费并入 ≤512 victim index、Consumed/Expired 残留 ring 按
      长度计费、orphan/corrupt/unsafe 保守计费并置 `spawn_blocked`、crash-`Running`
      经 ring 长度校验入 `recoverable_running`（不一致按 corrupt）；`recent_terminal`
      2× 缓冲后 compact 到 32、issue 计数 + 前 32 sample、文本各截 512 bytes。
      `SessionQuota`：counter/index/blocker 用叶子级 `std::sync::Mutex`，create 在锁内
      认领 victim、锁外按 victim commit lock 做 manifest-first 淘汰（无锁跨 await，锁序
      保持 quota 决策 → per-task commit 无环）；`reserve_for_create` 返回
      `ReserveOutcome{reservation, expirations}`，`QuotaReservation` Drop 回滚未 commit
      的预留；`finalize_terminal`/`finalize_consumed` recheck fully-consumed 强制
      Consumed（不生成 expiration），否则登记为 victim。测试覆盖 Retained/orphan 计费
      + blocker、按 terminal_at 最老优先淘汰并落 Expired、fully-consumed 落 Consumed 且
      释放预留、blocked inventory 拒绝 spawn。
      说明：非 canonical 名的 orphan 因类型化 API 只接受 `TaskId` 无法安全下降，按 0
      计费但仍置 blocker（增长仍被封闭）；这是相对文档"按可测量 regular file 计费"的
      安全等价简化。

- [x] **[registry] 用 `TaskOutputFiles` 替换内存 `TailBuf`**
      Purpose：消除常驻输出 bytes，并用 per-task commit lock 建立 manifest 线性化点。
      Verification：现有增量读/截尾测试迁移后断言不变；额外断言 registry 内不再保存
      stdout/stderr 正文 `Vec<u8>`；单次读取上限和 cursor 分段推进正确；barrier 测试
      两个并发 read 不重叠、Running read 不覆盖 terminal、release 不回退 cursor/status；
      manifest save 失败不返回 bytes 且 cursor 不前进。

- [x] **[process] 让 pipe pumps 写入 `DiskTail` 并加入 I/O failure 终态**
      Purpose：保持 leader/descendant/kill 语义，同时让磁盘错误诚实终止任务。
      Verification：leader 先退出后 kill、process group、setsid escape、有界 drain、
      stdout/stderr 流式读取测试全绿；pump EOF 不提前终止；创建失败不启动进程；
      stdout/stderr read/append failure 都 kill group 并提交唯一 `Failed`；child 先 exit 后
      pump failure 仍为 Failed；terminal manifest 失败不会卡住 keepalive/shutdown。

- [x] **[hub lifecycle] 接入 session-backed output root 和 archive restore**
      Purpose：证明未读输出跨 model switch、detach release/reopen、正常 server shutdown
      仍可按 task id 读取。
      Verification：集成测试覆盖 detached task 完成→release→reopen→notice→分段
      task_output；terminal summary 超 32 后 archive 仍可读；notice 已投递但未读取时文件
      仍存在；shutdown_all 返回时 ring flush 先于引用该 range 的 manifest save；只有
      校验通过的遗留 Running 转 Interrupted 并在一次 reopen 内立即落盘，ring/manifest
      不一致的 Running 进入 corrupt inventory 策略。

- [x] **[notice/tool] 区分 overwritten 与 lost 并更新模型文案**
      Purpose：无论模型只看通知还是调用 task_output，都能知道文件容量覆盖发生过。
      Verification：覆盖已读内容时 notice 报 overwritten、read 不报 lost；覆盖未读内容时
      两者分别报 storage total 和 consumer loss；同一 lost 不重复报告；checkpoint 已含
      `Completed(bg_x)` 后恢复 `OutputExpired(bg_x)` 仍会投递，反向亦然；同类 fact 重试
      去重；overflow batch key 跨 NoticeStore restore 稳定并覆盖 uncounted 去重。

- [x] **[cleanup/quota] 实现消费清理和 session 配额**
      Purpose：避免未读 archive 无界积累，同时不静默删除模型尚未读取的内容。
      Verification：64 MiB reservation 与配置下 active worst-case 的校验；active 永不
      淘汰；terminal 按 terminal_at 最老优先；create/quota check 串行；双 cursor 未到
      terminal total 时不消费清理；Consumed/Expired 都严格 manifest→delete→release；
      删除失败不释放 reservation；已投递旧 notice 后淘汰仍产生 OutputExpired，且
      `ReserveOutcome` 在 quota lock 释放后由 registry 入队，quota 层从不获取 notice/
      registry lock；task_output 返回 expired 而非 unknown；delete_session 全删；明确
      断言启动/attach inventory 不删除 crash orphan；Running 时读完、之后无新增输出再
      exit 会在 terminal 后进入 Consumed；零输出 task 直接进入 Consumed；barrier 覆盖
      terminal finalize 与 create 两种抢锁顺序，均不得产生 OutputExpired；cleanup failure
      仍发布 completion notice/summary，且 tail/overwritten 来自删除前快照。

- [x] **[presentation] 保持 byte cursor 并实现 UTF-8 carry**
      Purpose：分段读取不因字符边界回退持久化 cursor，也不重复 replacement character。
      Verification：0..=3 byte carry roundtrip；每个 UTF-8 byte boundary、terminal EOF、
      crash/reopen 和 consumer lost 组合测试；cursor 始终按 bytes 单调前进。

- [x] **[validation] 全量质量检查和真实 chatty process 冒烟**
      Purpose：确认磁盘 I/O 没有破坏 runtime/hub 时序或前端协议。
      Verification：`cargo clippy`、`cargo test`、`pnpm --filter coda-web lint`；持续输出
      任务并发 task_output、断线重连、kill、session 删除的手工流程。

### 实现说明（phase 4–10）

- **registry/process**：`BackgroundProcesses` 改为 `Store`（archive + `SessionQuota`），
  `new()=temporary()`、新增 `session_backed()`/`disabled()`；spawn 在 registry 锁下
  reserve+create（无环锁序）；read 在 per-task commit lock 下分段读、写游标后才返回
  bytes、drained terminal 触发 `finalize_consumed`；monitor 提交 terminal manifest →
  `finalize_terminal` → 发 notice/summary。pump 返回结构化 `PumpResult`，read/spool
  failure kill group 并提交唯一 `Failed`，cancellation biased 保持 `Killed`。read/kill
  用强类型 `TaskId` 返回 `Result`，tool 边界解析。
- **notice/tool**：`TaskNotice::Task` 带 `stdout/stderr_overwritten`，新增 `OutputExpired`，
  overflow 带稳定 `batch_id`；`UserOrigin`、restore dedupe 全部按
  `coda_core::llm::TaskNoticeKey`（fact 身份）。`task_output` 区分 overwritten(存储)
  与 lost(消费者)，Consumed/Expired 返回 note 而非 unknown。
- **hub lifecycle**：entry 首次 attach 惰性建 `session_backed`（rooted at
  `<session_dir>/background/tasks`，随 `delete_session` 递归删除），跨 detach/reopen/
  model switch/重启可读；inventory 重建 quota/blocker，恢复 recoverable Running→
  Interrupted；archive 打不开则 background disabled 但对话可用。
- **cleanup/quota**：消费清理由 cursor+terminal 双事件触发，Consumed/Expired 严格
  manifest→delete→release，删除失败保留 reservation；淘汰按 terminal_at 最老优先，
  fully-consumed 强制 Consumed；expiration 由 registry 在 quota 锁释放后入队。
- **presentation**：read 路径以持久化 `utf8_carry` 跨 chunk 边界拼接解码，byte cursor
  永不回退，terminal EOF/consumer loss 以 U+FFFD 收尾。
- **web**：`TaskStatus` TS 类型补 `Failed`/`Interrupted`，`taskStatusText` 相应渲染。
- **validation**：`cargo clippy --workspace --all-targets` 干净、`cargo test --workspace`
  全绿、`pnpm --filter coda-web lint` 干净、tsc 无错；新增 chatty-process 冒烟测试。
