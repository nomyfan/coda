# Background Task Execution — 设计方案(Step 2)

> 状态:v6(2026-07-11),**架构评审通过**(第五轮无 P0/P1)。v2:hub 生命周期、ID、回收、消息形态;v3:ownership 上移 hub entry、终态提交顺序、能力拆分、live 推送;v4:Owned/External、投递去重、NoticeStore 独立存储、锁序索引、绝对偏移;v5:投递保证收窄、NoticeStore Result + 原子写、溢出聚合、Owned shutdown 服从退出确认;v6:**崩溃语义改准为"可能丢失或重复"、save 失败为"丢新留旧"、确认 NoticeStore 随 delete_session 清理**。
> 前置:PR #33(cancel-aware tool execution)。后续:Step 3(sub-agent 后台化)不在本文范围;只有工具名是预留的稳定契约。

## Problem

让 `shell` 工具支持后台执行:长时间运行的命令不再占住一次工具调用,模型立即拿到任务 ID 继续工作,随后可增量读取输出、终止任务,并在任务结束时于下一个用户 turn 收到通知。后台任务须在断线、session release/reopen、模型切换之间全程存活;通知投递在**正常生命周期内恰好一次**(task-id 去重保证);server 崩溃可能**丢失或重复**通知——与 checkpoint 的 fire-and-forget 同一耐久层级。

## Scope

**In:**
- `shell` 新增 `run_in_background`(条件启用);
- hub entry 级注册表 `BackgroundProcesses`(spawn / 增量读 / kill / 通知 / 摘要 / 关停),终态提交顺序、回收上限、绝对偏移游标、**溢出聚合通知**;
- 新内置工具 `task_output`、`task_kill`;
- 通知 v1 投递:不自动唤醒,下一个用户 turn 注入历史;结构化 `UserMessage`(origin 携带 task ids),事件携带完整消息,fold 原样放置;恢复时按 task id 对照 checkpoint 去重;
- hub 生命周期:存活任务 keepalive、归零重判、release 时通知写入 server 侧独立 NoticeStore(**Result 接口 + 临时文件 rename 原子写**)、entry 初始化恢复恰一次、模型切换不触碰注册表;
- 已连接客户端的 live 摘要推送(attach 即得当前值)。

**Out:**
- sub-agent 后台化(Step 3);自动唤醒;任务跨 server 重启存活;TTL detached 保活;`task_output` 阻塞等待;
- **崩溃持久的投递保证**(prepare → checkpoint → ack 协议,含 `save_checkpoint` 返回 Result):被拒绝的升级路径——通知的耐久层级没有理由高于任务本身(崩溃时进程已成孤儿)与 checkpoint 系统(现状即 fire-and-forget);若将来 checkpoint 变为可靠提交,再启用该协议。

## Assumptions

- 单 workspace、单活跃会话(现有 SessionHub 模型)。
- 后台任务挂在 server 进程 + hub entry 上;Session 是注册表的借用者(External)或所有者(Owned),shutdown 责任跟随 ownership,且 **Owned 的清场以 runtime 确认退出为前提**。
- 输出的唯一逻辑消费者是本会话的模型(游标);dashboard 走 `summaries`,不碰游标。
- 任务规模小(Running ≤16,终态 ≤32),`Mutex` 足够。
- 破坏性变更可接受。

## Validation Findings

| 问题 | 方法 | 结果 | 设计含义 |
| --- | --- | --- | --- |
| 断线后 session 是否保活? | `hub.rs:565`、`hub.rs:546` | 无连接且 turn 不跑即 release 并 shutdown | keepalive 纳入存活任务;归零重判;通知落盘 |
| 模型切换如何处理 Session? | `hub.rs:713` handle_set_model | 先 open replacement 换入,再异步 shutdown 旧 Session | 注册表 owner 必须是 hub entry;restore 不跟随 Session open |
| runtime snapshot 由谁写? | `runtime.rs` save_agent_snapshot / wait_for_exit | runtime 多点反复整篇保存 | 通知放这里会被清零(双写者)→ server 侧独立 NoticeStore |
| Session::shutdown 的返回语义? | `session.rs:506`、`Shutdown::graceful` | 返回 bool;graceful+Return 超时返回 false(runtime 仍在跑) | Owned registry 只能在返回 true 后清场,否则半关闭 |
| session 删除会清理 NoticeStore 吗? | 评审核实 `delete_session()` | 递归删除整个 session 目录 | sidecar 通知文件随目录删除,无需独立清理流程 |
| hub fold 如何构造历史? | `hub.rs:304` | fold 精确复现 driver 写入;事件流不带 user 消息 | 通知事件携带完整消息,fold 原样放置 |
| 前端如何渲染 User 消息? | `protocol.ts:60`、`transcript.tsx:788` | 一律右侧用户气泡 | origin 标记恢复通知卡片 |
| 注册表如何到达工具? | `spec.rs:211` build() | BuildContext 逐 agent 构造,持有工具名单 | 注入 agent_name、句柄、allow_background_shell |
| driver 能否区分用户 turn? | `driver.rs:264` | `is_user_task` 已存在 | 通知注入复用 |

## Components

1. **`coda_tools::process`(重构)** — 抽出 `GroupedChild` 原语;前台语义与现有测试不变。
2. **`coda_tools::background::BackgroundProcesses`(新)** — 后台进程注册表:进程组生命周期、截尾缓冲(绝对偏移)、单一写者终态状态机、通知队列(含溢出聚合)、回收上限、摘要 watch、关停清场。
3. **`shell`(改)** — `run_in_background` 仅在 `allow_background_shell` 时进 schema;true 时立即以 "Started background task bg_…" 落定。
4. **`task_output` / `task_kill`(新)** — 薄壳;任何被授予者可构建(句柄恒在),与 shell 不绑定。
5. **wiring(改)** — `BuildContext` 增 `agent_name` / `background: Arc<…>` / `allow_background_shell`。`SessionBuilder::background(Arc)` 注入 = External;未注入自建 = Owned。`Session::shutdown` 对 Owned:**仅当本次 shutdown 确认 runtime 已退出(返回 true)时**执行 `registry.shutdown()`(杀净、join monitor,不持久化通知);超时返回 false 则注册表原样保留(任务活、可用),等后续 abort shutdown 完成清场。对 External 永不触碰。
6. **hub 生命周期(改)** — entry 初始化路径(新建 entry,含 release 后 reopen):创建注册表 → `NoticeStore::load`(损坏则告警、按空处理并把坏文件移侧)→ 对照 checkpoint 中 TaskNotice 的 task_ids 去重 → `restore_notices` 恰一次;同一 entry 内后续 open(模型切换、审批门控)只复用注册表。keepalive 三条件;归零重判 release:`session.shutdown` 完成后 `registry.shutdown()` 取回通知 → `NoticeStore::save` 整篇改写;save 失败则告警并继续 release(接受降级;rename 原子性保证旧文件完好——其中的陈旧通知经去重无害,但**本 entry 新产生的未投递通知随之丢失**)。
7. **`coda_server::NoticeStore`(新)** — 每 session 一份待投递通知的独立持久化,hub 唯一读写者;临时文件 + rename 原子写,永不半写。位于 session 持久化目录内,`delete_session()` 递归删目录时随之清理,无需独立清理流程(评审核实)。
8. **driver 通知注入(改)** — 根 agent 用户 `Task` envelope 处 `take_notices()`,在用户消息前写 `origin=TaskNotice{task_ids}` 的 User 消息,emit `AgentEvent::TaskNotice(UserMessage)`。
9. **`coda_server` + `coda_web`(改)** — fold 放置 TaskNotice;`protocol.ts` origin + 通知卡片(含溢出聚合的渲染);`ServerMessage::BackgroundTasks` 摘要推送(attach 发当前值)。

## Interfaces

```rust
// coda_tools::background

/// "bg_" + UUID v4(128-bit)。碰撞后果是杀错进程,不省熵。
pub struct TaskId(String);

impl BackgroundProcesses {
    /// 返回前已把任务计入摘要并发布 watch(keepalive 可见性先于 id 可见性)。
    /// spawn 失败、Running 达上限(16)或注册表已 closed 时返回 Err。
    pub async fn spawn(&self, cmd: Command, meta: TaskMeta) -> std::io::Result<TaskId>;

    /// 增量读(模型专用,推进绝对偏移游标)。未读内容已被截尾丢弃时,
    /// 返回可用尾部与丢失字节数,游标推进到 total_written。
    /// 未知或已回收的 id 返回 None("unknown or expired task")。
    pub async fn read(&self, id: &str) -> Option<TaskRead>;

    /// 请求终止:SIGKILL 进程组,等 monitor 提交终态后返回。终态只由
    /// monitor 提交(单一写者)。幂等;对已结束任务返回既有终态。
    pub async fn kill(&self, id: &str) -> Option<TaskStatus>;

    /// 取走累积通知。完整通知(含 ≤4 KiB tail)最多 64 条;更早溢出的
    /// 任务折叠进一条聚合通知(携带各自的 id + 终态,上限 256 之外只计数),
    /// 聚合槽本身不会被丢弃。
    pub async fn take_notices(&self) -> Vec<TaskNotice>;

    /// entry 初始化时把(已去重的)持久化通知重新入队。once-per-entry,
    /// 由调用方(hub)保证。
    pub async fn restore_notices(&self, notices: Vec<TaskNotice>);

    /// 全量摘要 watch:订阅即得当前值;hub 数其中 Running 做 keepalive。
    pub fn summaries(&self) -> tokio::sync::watch::Receiver<Arc<[TaskSummary]>>;

    /// 置 closed(拒绝后续 spawn)→ cancel 所有 Running → join 全部
    /// monitor(终态与通知提交完毕)→ 返回全部未投递通知。幂等。
    pub async fn shutdown(&self) -> Vec<TaskNotice>;
}

/// 完整终态通知,或溢出聚合(id + 终态列表,无输出 tail)。
pub enum TaskNotice {
    Task { id, command, description, status, output_tail /* ≤4 KiB */ },
    Overflow { dropped: Vec<(TaskId, TaskStatus)> /* ≤256 */, uncounted: u64 },
}
```

```rust
// coda_server(通知持久化 seam;hub 唯一读写者,once-per-entry)
trait NoticeStore {
    /// entry 初始化时读取;文件不存在视作空;损坏返回 Err(调用方告警、
    /// 移侧坏文件、按空继续)。
    async fn load(&self, key: &SessionKey) -> Result<Vec<TaskNotice>, NoticeStoreError>;
    /// release 时整篇改写为"仍未投递"集合(可为空)。临时文件 + rename,
    /// 失败返回 Err(调用方告警并继续 release;旧文件因原子性完好,
    /// 其中陈旧通知经去重无害;本 entry 新增的未投递通知丢失,接受降级)。
    async fn save(&self, key: &SessionKey, pending: &[TaskNotice]) -> Result<(), NoticeStoreError>;
}
// 刻意不放进 StoredRuntimeSnapshot:那份文件由 runtime 多点整篇重写,
// 加字段会被不知情的写者清零(双写者冲突)。
```

```rust
// coda_tools::spec
pub struct BuildContext {
    pub workspace_dir: String,
    pub todo_store: Arc<Mutex<Vec<TodoItem>>>,
    pub agent_name: String,
    /// 恒有:task_output/task_kill 无条件可构建(只授其一合法)。
    pub background: Arc<BackgroundProcesses>,
    /// 该 agent 同时被授予 task_output+task_kill 时为 true;
    /// 控制 shell 的 run_in_background 是否进 schema。
    pub allow_background_shell: bool,
}
```

```rust
// coda_core(消息形态)
pub enum UserOrigin {
    #[default] Human,
    /// 携带本条通知消息覆盖的任务 ids(含聚合通知内的 ids),供去重对照。
    TaskNotice { task_ids: Vec<String> },
}
pub struct UserMessage { /* ...现有字段..., */ pub origin: UserOrigin }

// coda_agent
pub enum AgentEvent {
    // ...
    /// 携带与 checkpoint 完全一致的消息对象,fold 原样放置。
    TaskNotice(UserMessage),
}

impl SessionBuilder {
    /// 注入外部注册表(External,hub entry 所有,Session 不管生死)。
    /// 未调用则自建(Owned):Session::shutdown 在 runtime 确认退出后清场。
    pub fn background(self, registry: Arc<BackgroundProcesses>) -> Self;
}
```

```rust
// shell 参数(仅 allow_background_shell 时保留在 schema)
run_in_background: Option<bool>,
// task_output:{ id } → 状态行 + 新增输出(含丢失字节说明)
// task_kill:  { id } → 终态描述
```

## Data Model

```
BackgroundProcesses (Arc;owner = hub SessionEntry 或独立 Session)
 └─ inner: Mutex<RegistryState>
      ├─ tasks: HashMap<TaskId, Arc<TaskEntry>>
      ├─ running_count: usize                    // 免反向锁的冗余索引
      ├─ summaries: HashMap<TaskId, TaskSummary> // registry-owned 副本
      ├─ terminal_order: VecDeque<TaskId>        // 终态回收顺序(最旧在前)
      ├─ monitors: HashMap<TaskId, JoinHandle<()>>
      ├─ notices: Vec<TaskNotice>                // 完整通知 ≤64
      ├─ overflow: Option<TaskNotice::Overflow>  // 聚合槽,永不被丢弃
      ├─ closed: bool                            // shutdown 后拒绝 spawn
      └─ summaries_tx: watch::Sender<Arc<[TaskSummary]>>

TaskEntry
 ├─ meta: TaskMeta { command, description, agent_name, started_at }
 ├─ state: Mutex<TaskState>
 │    ├─ status: Running | Exited { code, at } | Killed { at }
 │    ├─ stdout/stderr: TailBuf { bytes, start_offset: u64, total_written: u64 }
 │    └─ cursor: (u64, u64)   // 绝对输出偏移;cursor < start_offset ⇒ 报丢失字节
 └─ cancel: CancellationToken // 独立 token,不挂任何 turn token

终态提交协议(单一写者 = monitor)
 1. monitor 观察退出:锁 entry.state,Running → 终态,恰一次(kill 只 cancel)
 2. 锁 registry.inner:入队 TaskNotice(满 64 则把最旧完整通知降级并入
    overflow 聚合槽)→ running_count-- → terminal_order.push(超 32 则回收
    最旧:移出 tasks/summaries/monitors)→ 更新 summaries 副本(用第 1 步
    刚提交的值,不回锁 entry.state)
 3. 最后发布 summaries watch(归零对 hub 可见时,通知必已入队)
 spawn 对偶:锁 inner:closed 检查 → 插入 → running_count++ → 发布 → 返回 id
 锁序恒为 entry.state → registry.inner;持有 inner 时永不回锁 entry.state。

通知投递语义(v6 定稿)
 ├─ 保证:**正常生命周期内恰好一次**——完成入队 →(用户 turn)take →
 │   注入历史(随 checkpoint)→ release 时"仍未投递"整篇改写 NoticeStore;
 │   entry 初始化恢复 + task-id 去重消除该链上一切重复
 ├─ 崩溃语义:**可能丢失或重复**(与 checkpoint fire-and-forget 同级)。
 │   丢失:live entry 内存中的通知(未 take,或已注入但 checkpoint 未落盘
 │   且 NoticeStore 无该批)随崩溃消失——与孤儿进程同级。
 │   重复:恢复自 NoticeStore 的批次注入后、checkpoint 落盘前崩溃——事件
 │   先于 checkpoint 推给 dashboard(driver 先处理 envelope 后存盘),
 │   用户已见一次;重开后去重对照不到,再次投递
 └─ 升级路径(Out of scope):prepare→checkpoint→ack + save_checkpoint
     返回 Result;待 checkpoint 系统本身升级为可靠提交时一并做

内存上界:(16+32) × ~1 MiB + 64 × 4 KiB + 聚合槽 ≪ 50 MiB
```

## Load-Bearing Decisions

1. **后台工作活在工具调用语义之外**(调用即落定);checkpoint/重放/abort 零改动。
2. **注册表 owner = hub SessionEntry(External)或独立 Session(Owned),shutdown 责任跟随 ownership;Owned 清场以 runtime 确认退出(shutdown 返回 true)为前提**——graceful 超时返回 false 时任务与注册表原样保留,避免"Session 在跑、任务被杀、注册表 closed"的半关闭态。
3. **终态提交顺序是硬性协议**:monitor 单一写者;通知入队先于归零可见;spawn 先发布后返回;registry-owned 索引保证锁序单向。
4. **投递保证 = 正常生命周期恰好一次;崩溃可能丢失或重复**(v6 定稿:v5 的"不可见重"被反例推翻——通知事件在 checkpoint 落盘前已推给 dashboard,恢复批注入后、落盘前崩溃会造成可见重复)。理由:通知的耐久层级与任务本身(崩溃即孤儿)、checkpoint(fire-and-forget)对齐;拒绝的替代(prepare/ack 协议)记入 Scope Out。NoticeStore 用 Result 接口 + 原子写,失败策略显式(load 损坏按空 + 移侧;save 失败继续 release,陈旧通知去重无害、本 entry 新通知丢失)。
5. **v1 不自动唤醒**;结构化消息 + 事件携带完整对象 + fold 原样放置;live 走 `summaries` transport 推送。v2 自动唤醒接缝:通知作为 envelope 投递。
6. **内部命名如实 `BackgroundProcesses`**;只有工具名为 Step 3 预留。
7. **句柄与授权拆分**:句柄恒在 BuildContext,`allow_background_shell` 单独判定;部分授权是特性。
8. **有界通知队列 + 溢出聚合**:完整通知(含 tail)≤64;更早的降级为聚合通知(id + 终态,≤256,再溢出只计数,聚合槽永不被丢)——终态"事实"的保留强于输出细节。
9. **任务 id 128-bit 随机;截尾缓冲(绝对偏移)+ 回收上限**,内存有界,丢失显式可见。
10. **每任务独立 `CancellationToken`**:turn abort 不杀;`task_kill`/registry shutdown 才杀。

## Risks / Open Questions

1. **最大风险:hub 生命周期闭环未经验证**——keepalive、归零重判、NoticeStore 读写时机、entry 初始化恢复、模型切换 swap 保活的时序。对策:第一步假任务打通全部路径。
2. 重构 `process.rs` 动摇前台不变量。对策:原语抽取单独成步,现有测试零改动全绿。
3. fold 顺序:`stale ToolCallEnd → TaskNotice → 用户消息` 的写入序与放置序逐字段一致;单测锁定。
4. 连续两条 user 消息对个别 provider 的兼容性:降级只动 provider adapter 层,绝不合并持久化消息。
5. 崩溃孤儿进程:接受;必要时后续加启动 sentinel 特征扫描。
6. (v2 自动唤醒前置)归零即 release 与"来通知需要活着的 agent"张力:届时选"未投递通知算保活理由"或"hub 收通知 reopen 再投递"。

## Implementation Roadmap

- [x] **[lifecycle] 假任务打通 hub 闭环(风险最前置)**:注册表骨架(假任务,终态提交协议按真实实现)、`summaries` watch、Owned/External、keepalive + 归零重判、NoticeStore(Result + 原子写)、entry 初始化恢复(去重)
      Purpose:验证四轮评审全部 P1 的集中地。
      Verification:集成测试:① 启任务→settle→断线→完成→归零 release(NoticeStore 落盘)→重连 reopen→下一 turn 收到通知;② 运行中断线不 release;③ 运行/已完成未通知时切模型,任务、输出、通知不丢;④ 同一 entry 多次 open 只 restore 一次;⑤ barrier:通知入队先于归零可见 / kill 与自然退出恰一终态一通知 / shutdown join monitor 后返回通知 / spawn 返回前 keepalive 非零;⑥ Owned:独立 Session shutdown(确认退出)后无进程残留;⑦ **Owned + graceful 超时返回 false:任务仍活、注册表仍可用,随后 abort shutdown 完成清场**;⑧ NoticeStore:save 中途失败不损坏旧文件;损坏文件 load 按空并移侧。
      **状态:已完成**(2026-07-11,`coda_tools::background` + `SessionBuilder::background` + hub keepalive/NoticeStore/idle watcher)。①的"下一 turn 收到通知"暂以 reopen 后注册表 `take_notices` 断言代替,driver 注入属第 6 步;④去重对照 checkpoint 同样待第 6 步 origin 落地(现以 TODO 标注)。实现评审补修四处:恢复的聚合通知并入 overflow 槽而非完整队列;门控 reopen 失败改走 `begin_release`(裸移除会泄漏 idle watcher 与 relay 流);`shutdown` 加互斥门串行并发调用;`kill` 等待 summaries 发布(完整提交)而非仅终态翻转。另证实既有 runtime 限制:Exit 被消费后 driver 等待 run_fut 期间,后续 Abort 无法抢占(见 `AgentRuntime::wait_for_exit` 的 TODO)。
- [x] **[process] 抽取 `GroupedChild` 原语**
      Verification:现有测试零改动全绿;clippy 干净。
      **状态:已完成**(2026-07-11)。sentinel-first spawn、入组、killpg、disarm 与 Drop 兜底收拢为 `GroupedChild`(`pub(crate)`,stdin null + stdout/stderr piped);`run_command` 改为其首个调用方,原 `KillGroupGuard` 缩减为只负责 reader abort 的 `AbortReadersGuard`(组的 Drop 兜底移入 `GroupedChild` 自身,killpg 在字段析构前执行,sentinel 仍钉住组)。coda_tools 40 测试零改动全绿。
- [ ] **[registry] 接真进程**:监视任务、TailBuf(绝对偏移)、回收、通知上限与聚合
      Verification:单测:spawn→增量读→退出出通知;kill 全组;截尾后 read 报丢失字节且不重复不跳过;setsid 逃逸不阻塞;终态超 32 回收;**通知超 64 降级聚合、聚合超 256 计数、聚合槽不可丢**;shutdown 无残留。
- [ ] **[tools] `shell` 条件 schema;`task_output`/`task_kill`;注册 builtin**
      Verification:成套判定;观察者 agent 可构建;增量/幂等/expired 文案。
- [ ] **[wiring] `BuildContext` 三字段 + build 判定 + hub entry 注入链路**
      Verification:abort 不杀;切模型不杀且 task_output 连续可用;release 后无残留。
- [ ] **[driver] 注入 `origin=TaskNotice{task_ids}` + `AgentEvent::TaskNotice` + checkpoint**
      Verification:注入顺序;事件对象与历史逐字段一致;checkpoint 与事件流一致。
- [ ] **[server/web] fold 放置、`protocol.ts` origin、通知卡片(含聚合)、摘要推送(attach 发当前值)**
      Verification:lint;fold 顺序单测(`stale ToolCallEnd → TaskNotice → Human User`);手工:启 dev server→断线重连(列表立现)→task_output→切模型(任务在)→kill→通知出现在下一轮且刷新后仍为卡片。
