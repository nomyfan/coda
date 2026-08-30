## Problem

后台任务的完成通知、进程和输出 archive 没有共享同一个可靠的释放边界，导致竞态丢通知、entry 释放后遗留进程、删除期间新 attach 使用即将被删除的 spool、server 重启后输出不可恢复，以及 writer 生成 reader 拒绝的 manifest。

## Scope

In：修复 hub release 的通知判定与 registry shutdown；给 session-backed spool 配置稳定根目录；让 session 删除在 hub tombstone 内完成 runtime、registry、数据库和 spool 清理；统一 manifest 的读写大小上限；补充对应回归测试。

Out：跨主机共享 spool、接管崩溃后仍存活的孤儿进程、为未投递通知新增持久化协议、自动清理未知旧 spool root。

## Assumptions

- 一个 background spool root 同时只允许一个 server 进程写入；启动时必须取得 root lock，否则启动失败。现有 archive 不支持 active-active。
- session id 和 workspace id 继续作为 spool 的定位键；删除数据库 session 和对应 spool 是同一个 hub 删除事务的持久化阶段。
- hub 持有 idle entry mutex 时，生产调用链不能开始新的 spawn/read/kill；需要等待的只有已登记的 detached activity 和既有 task monitor。测试直接持有 registry handle 的并发操作不代表生产所有权模型。
- forced resync、显式删除和进程 shutdown 可以终止尚在运行的后台任务；它们产生但尚未投递的 completion notice 仍沿用当前 best-effort 语义。正常 server shutdown 后，重启可以读取被保存为 `Killed` 的状态和输出，但不会补发 completion notice。
- 本次保证的是跨正常 server 重启，不保证跨系统重启或系统临时目录清理；默认 root 可以位于系统临时目录，但路径不能包含 PID，正常退出也不能删除。

## Validation Findings

- `SessionBuilder::background` 注入的 registry 是 External，`Session::shutdown` 明确不会关闭；hub 必须持有并关闭它。
- monitor 在 registry 锁内依次 enqueue notice、running 归零、publish summary；hub 可以通过一个 registry 接口等待 detached archive/quota activity 收敛，再在 registry 锁内同时判断 idle 并提取 notice，消除 watcher/detach 与取消 waiter 的竞态。
- `SessionQuota` 和 `TaskArchive` 都已有 activity barrier；`shutdown()` 会依次 `quota.settle()`、`archive.settle()`，再提取 quota expiration 和 registry notice。普通 idle release 必须复用同一收敛语义，不能只看 `running_count`。
- PostgreSQL session 没有文件目录可挂载 spool；当前 PID temp root 在正常退出时删除，崩溃残留也不会被新 PID inventory。
- `read_manifest` 有 64 KiB 上限，`save_manifest` 无上限；shell 的 command/description 也没有长度约束。
- `open_session` handler 需要 durable model binding 才能选 provider，因此在进入 hub 之前就调用 `initialize_session` 插入 session 行。这一步不在 tombstone 内：并发 delete 可以删掉刚插入的行，attach 随后在 tombstone 之后开出一个没有 session 行的 live session，其后所有 checkpoint / snapshot 写入都会违反外键。E2E 中 6/6 轮复现。

## Alternatives Considered

### Spool 存储位置

选择：默认使用 `std::env::temp_dir()/coda-background`；可选的 `[background].root` 允许部署覆盖为持久卷，相对路径按 `coda-server.toml` 所在目录解析。其下继续使用 `<workspace_id>/<session_id>`。

- 相比把 ring bytes 放进 PostgreSQL，它保留现有有界磁盘 ring、fd-relative 安全边界和流式读写，不把高频输出写入数据库。
- 稳定的默认临时路径足以覆盖正常 server 重启，且不增加配置门槛；接受系统重启或平台清理策略可能删除未读输出。
- 可选覆盖不会污染用户代码仓库，并允许运维把大文件放到独立卷或要求更强持久性。

### Root 单写者

选择：增加专用 `BackgroundRootLock::acquire`，不复用当前会 `create_dir_all` + `fchmod` leaf 的 `ArchiveDir::open_or_create_root`。它先确保 parent 存在，再以 `mkdir(0700)` 创建 leaf；新建分支中 umask 只能收紧权限，随后以 `O_NOFOLLOW` 打开、验证 type/owner，并收紧和复核为精确 `0700`。若 leaf 已存在，则只以 `O_NOFOLLOW` 打开并严格验证 type/owner/mode，错误 mode 直接失败，绝不自动 chmod。验证 root 后，再通过该目录 fd 以 `openat(O_CREAT | O_NOFOLLOW)` 创建或打开固定 `.lock`。锁文件必须是当前用户拥有的普通 `0600` 文件；验证后取得 non-blocking advisory exclusive lock，并同时持有目录 fd 和文件 fd 到进程退出。默认 root 和配置覆盖使用同一机制，锁冲突直接启动失败，运行期间没有删除或替换 `.lock` 的代码路径。

相比只按 config/database 做命名空间，root lock 同时覆盖滚动重启的重叠窗口和两个实例误用同一显式路径；命名空间只能降低碰撞概率，不能证明单写者。相比 PID lock directory，内核 file lock 会随进程退出自动释放，不需要判断或清理 stale PID。

### Release 判定

选择：由 `BackgroundProcesses` 提供“等待既有 detached activity 收敛；若无 running task，则 drain quota expirations 和 registry notices”的接口；hub 不再分别读取 watch snapshot、quota facts 和 notice queue。

相比在 hub 调换两次读取的顺序，这个接口把 activity barrier、expiration staging 和 enqueue/running-count 的并发约束封装在 registry 内，调用点不需要依赖 publish 顺序推理。

## Components

- `BackgroundProcesses`：等待 archive/quota activity 收敛后提供 idle + pending notice 状态，并在 shutdown 时形成 kill、join、flush、drain 屏障。
- `SessionHub`：entry release 统一执行 runtime shutdown、external registry shutdown、map removal；entry delete 额外保留 `Deleting` tombstone，串行化 attach 与完整删除事务。
- `BackgroundRootLock`：使用专用的严格 root opener 建立 fd-relative/no-follow 边界，在任何 session archive 打开前安全取得 root 级进程独占锁，并持有至 server 退出。
- `ServerConfig` / `AppOpener`：加载稳定 background root，并按 session key 定位 archive；在删除事务中删除数据库 session 和 archive 目录。
- `TaskArchive`：在任何 temp file 写入或 rename 之前拒绝超过 reader 上限的序列化 manifest。

## Interfaces

```rust
impl BackgroundProcesses {
    /// 等待当前 detached archive/quota activity 收敛；`None` 表示仍有 running task，
    /// `Some` 表示 registry quiescent，并返回 quota 与 registry 当前全部 notice。
    pub async fn take_notices_if_quiescent(&self) -> Option<Vec<TaskNotice>>;
}

pub struct BackgroundConfig {
    /// 稳定 spool 根目录；缺省为系统临时目录下不含 PID 的固定路径。
    pub root: PathBuf,
}

impl BackgroundRootLock {
    /// 安全创建或严格打开 root，随后 fd-relative 创建或打开 `.lock`，
    /// 验证两者的 owner/type/mode，并取得 non-blocking 进程级独占锁。
    pub fn acquire(path: &Path) -> Result<Self, ArchiveError>;
}
```

可选的 `[background].root` 是配置文件信任边界：加载时执行环境变量展开和相对路径解析；archive 层继续负责目录权限、no-follow 和 session key 路径安全。

## Data Model

每个 hub entry 独占一个 external `Arc<BackgroundProcesses>`。稳定磁盘层级为：

```text
background.root/
  .lock
  <workspace_id>/
    <session_id>/
      <task_id>/
        meta.json
        stdout.ring
        stderr.ring
```

正常 release 和 server shutdown 保留 session 目录；显式 session delete 在 `Deleting` tombstone 内先把 `<workspace_id>/<session_id>` rename 到 root 下的 `.trash/`，再删数据库行，最后 best-effort 递归删除被隔离的副本。`.trash` 与 `.lock` 一样以 `.` 开头，而 workspace id 不允许以 `.` 开头（`config::is_workspace_id` 在启动时拒绝，`background_dir` 再校验一次），因此隔离目录不可能再被任何 session 打开。session id 位于 workspace 目录之下，不与任何保留名冲突，因此仍只按 `storage::validate_session_id` 的规则校验——两处规则必须一致，否则 API 接受的 session 会无法 spool，并且连删除都会失败。

## Load-Bearing Decisions

- background root 使用专用 opener：新 leaf 请求 `0700` 创建并在新建分支收紧、复核；已有 leaf 以 `O_NOFOLLOW` 打开并严格验证 owner/type/精确 `0700`，不修复错误 mode。`.lock` 只允许通过已验证目录 fd 安全创建/打开，同样验证 owner/type/精确 `0600`。root capability 和 lock fd 在 hub 构造前取得并持有到 server 完全退出；任何 lock 冲突或 symlink/非普通文件都是启动失败。
- server 运行期间绝不 unlink、rename 或 recreate `.lock`；锁 inode 保持稳定，进程退出由内核释放 file lock。
- registry shutdown 发生在 runtime 完全停止之后、entry 从 map 移除之前，保证没有 agent 再 spawn，同时 reopen 不会与旧 monitor 的 manifest 写入竞态。
- 显式 session delete 的实际工作跑在 hub 自己 spawn 的 task 里，调用方只订阅 watch 结果。tombstone 只能由删除自身完成来解除，若把 future 绑在调用方身上，被 abort 的请求 task（例如连接断开）会留下永远无法解除的 `Deleting`，之后每次 attach 都在它上面空转。
- spool 清理排在数据库删除之前，且以 rename 为准：只有 canonical 路径消失才算成功，rename 失败即整体失败，不删任何数据库行。留在 `root/<workspace_id>/<session_id>` 的内容会被下一个同 id 的 session 整个继承（继承已删除 session 的任务与输出，或因半删 archive 而无法启用 background），这是状态串线而非磁盘泄漏。rename 之后的递归删除才是 best-effort。代价是更罕见的失败方向：数据库删除失败时 session 仍可重开但丢失已完成任务的输出。
- 显式 session delete 先把 entry 变成 `Deleting` tombstone；runtime、registry、spool 和数据库清理全部完成后才从 map 移除并唤醒 attach。数据库清理失败也必须释放 tombstone，让后续 attach 可以重新打开仍存在的 session；没有 live entry 的删除同样要先创建 tombstone。
- `AppOpener::open` 在 hub entry 锁内幂等重建 session 行（`initialize_session` 是 insert-on-conflict-do-nothing）。handler 侧的调用只用于读取 binding；行的存在性由 tombstone 之后的这次调用保证。
- 普通 release 先等待 quota/archive activity barrier，再在 registry mutex 下检查 running count 并 drain quota expiration 与 notice；entry mutex 仍先于 registry 内部锁，保持现有锁序。
- `Releasing` entry 的 `done` 只在 runtime 和 registry 两个 shutdown 都完成、map entry 已移除后发布；`shutdown_all` 遇到 already-Releasing 时等待 `done`，不能跳过。
- manifest 上限在 writer 和 reader 两端使用同一常量；writer 超限时不创建/替换 `meta.json`，initial create 走现有 rollback，因此不会启动进程。
- 正常 server shutdown 不删除 background root；显式 session delete 仍是唯一整目录删除路径，且该路径由 hub 的 `Deleting` tombstone 覆盖到目录清理完成。
- 正常 server shutdown 产生的未投递 completion notice 不持久化、不在重启后重建；验收只要求 terminal manifest/output 可读。需要补发通知时必须另行设计 notice 持久化协议。

## Risks / Open Questions

- 默认临时 root 只保证正常 server 重启；要求跨系统重启或抵抗临时目录清理的部署必须显式配置持久路径。
- advisory lock 依赖本项目支持的 Unix 本地文件系统语义；不承诺在忽略/弱化 file lock 的网络文件系统上安全运行。同一 Unix 用户主动替换整个已打开 root path 仍不在本次防护范围内。

## Implementation Roadmap

- [x] [registry] 增加原子 idle/drain 接口并用竞态回归测试固定语义
      Purpose：先关闭完成通知丢失窗口。
      Verification：watcher 被 entry 锁阻塞时，release 判定仍提取 notice 且拒绝释放；取消 waiter 留下的 quota transaction 完成 expiration staging 前不得判定 quiescent，完成后 fact 必须被 drain。
- [x] [hub] 让所有 entry teardown 经过 external registry shutdown 屏障
      Purpose：确保进程组和 monitor 在 entry/map/spool 消失前收敛。
      Verification：分别覆盖 detach、delete、force_resync、abandon/open failure、stream-ended、server shutdown 和 already-Releasing；每条路径都断言 registry shutdown 完成前 map entry 仍存在，完成后 registry 拒绝新 spawn 且 map entry 消失。
- [x] [server root] 接入稳定的默认 root、可选覆盖和进程独占锁，删除进程退出时的 root 清理
      Purpose：让新进程能 inventory 同一 session archive。
      Verification：配置解析测试覆盖默认临时路径、相对覆盖和绝对覆盖；新 root 最终为精确 `0700`；已有 root 的 symlink、非目录、错误 owner/mode，以及 `.lock` 的 symlink、非普通文件和错误 owner/mode 均被拒绝；子进程取得 lock 并报告 ready 后，父进程取得同一 root 失败，子进程退出后父进程可重新取得，以同时覆盖跨进程排他和异常退出后的内核自动释放；reopen 恢复测试继续通过。
- [x] [restart semantics] 固定正常 shutdown 的 notice/output 边界
      Purpose：让 best-effort 通知语义成为显式产品行为而非实现偶然。
      Verification：运行任务经 registry shutdown 保存为 `Killed`；新 registry 可读取状态和输出，但 `take_notices()` 为空。
- [x] [archive] writer 使用与 reader 相同的 64 KiB 上限
      Purpose：禁止生成自身无法重开的 manifest。
      Verification：超大 TaskMeta 创建失败、task 目录回滚，边界内 manifest 正常创建。
- [x] [session delete] 将数据库与 spool 删除纳入 hub 的 `Deleting` tombstone
      Purpose：消除 `relay.delete()` 返回、数据库删除和 `remove_background_dir()` 之间允许新 attach 的窗口；覆盖原本没有 live entry 的删除。
      Verification：删除事务未完成时 attach 必须等待；数据库或 spool 清理完成后 entry 才能消失；清理失败后 attach 可以重新打开保留的 session；新 attach 不会继续使用随后被递归删除的 archive；opener 的 `open` 必须排在 `delete_persisted` 之后，使并发 open 不会留下没有 session 行的 live session。
- [x] [delete ownership] 把删除工作交给 hub 自持的 task，调用方只等待 watch 结果
      Purpose：调用方被取消时不能让 `Deleting` tombstone 永久占住 key。
      Verification：delete 进入 `delete_persisted` 后 abort 调用方，清理仍须完成、tombstone 须消失、随后 attach 能重新打开 session。
- [x] [spool quarantine] 删除时先 rename 走 canonical spool 路径，再删数据库行
      Purpose：`Deleted` 不得在旧 spool 仍位于 canonical 路径时返回，否则同 id 的新 session 会继承它。
      Verification：retire 之后 canonical 路径必须不存在；未创建过 spool 的 session 不算失败；隔离副本删除失败时 retire 仍可成功，但报告成功必然意味着 canonical 路径已释放；`.trash`/`.lock`/`..`/含 `/` 的 workspace id 一律不可寻址，且启动时即报错；`validate_session_id` 接受的 session id（含 `.foo`）必须能正常 spool 与 retire。
- [x] [relative root] 固定单组件相对 root 的行为
      Purpose：`background.root = "background"` 配在裸文件名的配置旁时解析出的 parent 是空路径。
      Verification：子进程在临时工作目录下以 `BackgroundRootLock::acquire(Path::new("background"))` 取锁成功并创建 `background/.lock`（`create_dir_all("")` 本身返回 `Ok`，无需改动代码）。
- [x] [validation] 运行 workspace formatter、clippy 和 tests
      Purpose：验证跨 crate 接线和既有行为不回退。
      Verification：`cargo clippy --workspace --all-targets` 与 `cargo test --workspace` 通过。
