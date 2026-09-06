# coda_agent process 模型设计

## Problem

拆开 `Agent` 的定义与实例职责，让维护者直接通过 program、process 和执行分组理解路由、并发、恢复与取消。需求见 [agent-process-model](../requirement/agent-process-model.md)。

## Scope

- 本轮完成内部结构整理及 `coda_process` → `coda_execution`，保留已有可观察行为。
- 不新增模型可调用的 spawn/send/wait 工具，不改变 stateful/stateless、后台启动资格、审批、通知和 fork/rewind 规则。
- source code 是 `AgentSpec` 的概念定位，不增加名为 `SourceCode` 的包装类型。

## Validation Findings

以下结论来自静态读码；本轮未运行测试或修改实现。

- `spec.rs::AgentTeam::new/build` 已分别完成声明校验和工具构建；build 目前还提前分配了实例状态，可去除这部分。
- `runtime.rs::bootstrap/start_driver` 已按 thread 创建独立 driver；`Agent::for_thread` 分配独立状态。不是每个 program 只有一个串行 driver。
- `driver_tests/concurrency.rs` 在模拟模型中使用双参与者 barrier，覆盖同名 stateless agent 实际并发；重构必须保留这种验证强度。
- `AgentTeam::build` 为工具绑定 session 的 `BackgroundTasks`、workspace 和共享文件锁。Program 不能简单提升成跨 session 的全局对象。
- `execution.rs`、`runtime/scopes.rs` 用 `(thread_id, invocation_id)` 记录执行成员；前台 scope 按 turn、后台按 task，持久化清理依赖这一执行身份。
- `persist.rs::StoredCheckpoint` 已区分持久实例关系与临时执行状态；工具状态锚定历史消息，不能另建与 fork/rewind 脱节的独立存储。

## Alternatives Considered

| 决策 | 备选 | 选择与代价 |
| --- | --- | --- |
| 定义对象 | runtime 直接持有展开的配置，或独立 Program | 保留 Program，集中表达构建结果；仅为数据对象，不加消息转发层 |
| 实例承载 | 每个 process 常驻 Tokio task，或 driver 按需恢复 | 按需恢复，延续现有资源回收；必须区分持久身份与在线 handle |
| 分组寿命 | process 永久属于某组，或每次 execution 指定组 | execution 指定组，保持 stateful 跨轮复用及现有取消行为；不是严格的 Unix process group |
| 分组实现 | 新建通用 GroupManager，或整理现有 executions/scopes | 整理现有运行时登记与清理，不引入独立管理服务，也不强行统一前后台所有细节 |
| 调用方式 | 本轮改成显式 pid API，或保留名称寻址和复用 | 保留外部调用方式，内部把实例选择与运行拆开；新工具 API 留作后续行为变更 |
| 持久化命名 | 全部 thread 字段改成 process，或仅重构运行时 | 保留现有 SQL/协议字段名称，避免无行为价值的迁移；内部使用 ProcessId，边界显式转换，不提供旧 Rust 类型兼容别名 |

## Components

- `AgentSpec` / `AgentTeam`：保留声明数据、图校验和 session 绑定入口；`build` 返回按名称索引的 `Arc<Program>`，不再创建空 AgentState。
- `Program`（`program.rs`）：持有名称、SystemPrompt、Tools 和已解析的 subagent 调用定义；可由同一 session 中多个 process 引用。
- `Process`（`process.rs`）：持有唯一 ProcessId、Program 引用、历史及派生状态、执行恢复点；提供记录、恢复和构造模型输入等操作。
- `ProcessRuntime`（现 `AgentRuntime`）：拥有 program 表、在线 process handle 表、执行登记和事件广播；负责实例选择、启动、路由及取消，保留为 crate 内部实现。
- process driver（现 `runtime/driver.rs`）：驱动一个固定 pid 的 Process；接收该实例的 inbox 和控制通道，推进生成、工具执行、审批和回复。
- process group（整理现 `execution.rs`、`runtime/scopes.rs`）：表达本次执行的取消归属及成员清理，成员按 execution 身份记录；前后台共享身份模型，保留不同完成处理。
- `Session`：继续提供用户输入、审批恢复、事件读取和关闭的应用接口，管理 root 和所有执行分组。
- `coda_execution`：保持现有 OS 子进程执行、后台任务注册及输出归档职责；不反向依赖 `coda_agent`，不承接 agent 调度。

## Data Model

```text
AgentTeam（已校验 AgentSpec）
  └─ build(session resources) → programs[name]: Arc<Program>

Session / ProcessRuntime
  ├─ programs
  ├─ processes[pid]: ProcessHandle          在线 driver，不代表全部持久实例
  ├─ executions[pid]: ActiveExecution      每个 pid 最多一个当前执行
  └─ groups                               执行成员、取消和清理状态

Process（由对应 driver 推进）
  ├─ pid + Arc<Program>
  ├─ history + derived tool state
  └─ resume point / pending calls / reply target

ActiveExecution
  ├─ identity: (pid, invocation_id)
  ├─ group: Foreground(turn_id) | Background(task_id)
  └─ completion: RootTurn | Caller(reply target) | BackgroundTask(task_id)
```

- `ProcessId` 接替 `ThreadId` 的运行时职责，保留现有值和派生算法；root pid 继续使用 session id，不引入 OS PID 的整数约束。
- `Program` 不持有消息、执行状态、inbox 或 driver；SystemPrompt 的动态绑定仍按现有规则求值。它不是冻结的 prompt 字符串，也不承诺工具对象内部完全不可变。
- Program 绑定范围是单个 session。文件锁继续由服务进程共享；后台注册表由 session 共享；知识热更新句柄保持现有 workspace 共享范围；model profile 和动态权限仍由运行配置提供。
- 历史是权威数据，当前 LLM 上下文由 message view 派生，工具状态从消息锚点归约。这里不把完整历史、模型可见 working memory 和执行恢复点合并为同一概念。
- 活跃 memory 仍可用 `Arc<Mutex<...>>` 供 live snapshot 读取；driver 是唯一历史修改者，工具调用通过结果提交状态，保留 batch-start snapshot 隔离。不为追求单所有者取消现有实时读取能力。
- group 登记持有取消与准入信息，Process 持有恢复状态；checkpoint 中的 execution 元数据是持久副本，只通过现有保存/清理路径更新，不能形成两个独立可写的事实来源。

## Interfaces

以下记录内部接口及职责边界；参数结构沿用现有任务内容、来源和结果类型，不新增面向 LLM 的 API。`AgentTeam::build` 的资源参数保持原有形状。

```rust
// 根据已校验声明和 session 资源构建可复用程序，不分配实例 memory。
fn AgentTeam::build(...) -> HashMap<String, Arc<Program>>;

// 在当前待执行批次产生启动副作用前，找出所有重复 stateful 目标名称。
// 纯检查，不占用实例；driver 为命中这些目标的调用逐一记录拒绝结果。
fn preflight_stateful_calls(
    program: &Program, calls: &VecDeque<PendingToolCall>,
) -> HashSet<String>;

// 验证调用目标及启动资格，选择新实例或现有 stateful 实例并提交工作。
// 仅接收经过 driver 批次预检的调用；仍检查实时 busy 等准入条件。
// 返回前台回复关联或后台 TaskId；退出中、未知目标、实例忙或清理阻塞时失败。
async fn ProcessRuntime::invoke(
    &self, caller: &ProcessId, call: SubagentInvocation,
) -> Result<InvocationReceipt, String>;

// Running 时投递或恢复已登记 process；Exiting 时将已准入执行的消息保存到 snapshot。
// Exiting 不启动 driver；Closed 才拒绝交付。成功表示接收，不保证退出期落盘成功。
async fn ProcessRuntime::deliver(
    &self, envelope: Envelope,
) -> Result<(), SendCommandError>;

// 关闭后台组准入并停止其执行；持久清理由 runtime 跟踪并重试。
// 返回不代表持久清理结束；has_background_work 在清理完成前保持 true。
async fn ProcessRuntime::stop_background_group(
    &self, task_id: &TaskId, error: Option<String>,
);
```

- `InvocationReceipt` 区分前台待回复关联与后台 TaskId。前台 driver 先提交整批调用，再等待所有 pending replies，不在循环中等待每个子调用完成。
- 批次预检由持有完整待执行队列的 driver 负责，在提交该批任何调用前完成，范围与现有 `concurrent_stateful_subagents` 一致。依据已解析 Program 的目标及 mode 检查重复，不先过滤参数解析失败的调用；同一 stateful 目标的所有重复项均拒绝，前后台标志不同也不例外。stateless 重复项及其他目标继续正常处理。
- 预检不预留 pid、不启动 driver 或创建后台任务，也不将整批变成事务。`invoke` 仍是唯一的逐调用运行准入入口，检查与其他批次或已有后台执行的实时冲突；预检通过不保证后续准入成功。恢复后的待执行批次在重新提交前同样预检，不重复提交已完成或已在等待回复的调用。
- `invoke` 中实例选择沿用现有规则：stateless 按调用来源派生 pid，stateful 按父 pid 与目标名称派生 pid。新实例建立记录，已有实例加载状态；两条路径汇入同一提交执行流程。spawn 仅指新实例创建，不能把 stateful 的每次调用都解释成 spawn。
- 单个固定 pid 的 driver 不再选择其他 pid 的数据。内部恢复入口接收已解析的 process/checkpoint；移除仅用于“当前选择哪个 thread”的状态，保留等待回复、审批与恢复阶段状态。
- `Envelope` 继续表示消息，目标运行身份是 pid。名称用于 program 选择或事件展示，不作为第二套路由键；恢复时从登记或 checkpoint 确定 Program。可持久化的 envelope 仍保存足够的来源和执行关联。
- 继续使用有界 mpsc inbox、控制通道和 broadcast 事件流；不引入通用 Pipe 抽象。Reply/Resume 可以推进等待中的执行，busy 限制针对新任务，不能阻塞完成消息。

退出期间的交付契约（逻辑阶段，不要求新增独立 lifecycle 管理对象）：

| 阶段 | 新调用准入 | 已准入执行的消息交付 |
| --- | --- | --- |
| Running | 按正常策略检查 | 投递 inbox，必要时恢复 driver |
| Exiting | 停止新的用户任务及 subagent 调用准入 | runtime 接收有效的在途 envelope，按目标 pid 写入恢复 snapshot 并尝试持久化，不重新启动接收方 driver |
| Closed | 拒绝 | 运行资源已释放，返回关闭错误，不再承诺接收 |

- Exiting 的保存职责在 runtime 的 `deliver`，不依赖父 driver 存活；前台子调用在 graceful shutdown 期间完成的 Reply 必须走此路径，不能因父 handle 已回收而当作目标不存在拒绝。执行有效性检查保留 checkpoint/pending reply 关联，不能仅以在线表是否有目标判断。
- 退出切换与消息交付需要有明确先后顺序：已入 inbox 的消息由 driver 退出快照收集；进入 Exiting 后的消息由 runtime 缓存。snapshot 更新及最终保存不得相互覆盖。到 Closed 前应等待消息生产者退出并完成已有收集/保存流程；有界强制终止仍遵循现有取消和清理规则，不保证未生成结果的交付。
- 交付锁不覆盖等待 inbox 容量：先在锁内选择接收方，锁外 reserve 容量，再回到锁内检查生命周期并同步入队。等待期间若进入 Exiting，不论 reserve 成功还是接收方已关闭，都转入归档；进入 Closed 则拒绝。不能用跨 process 的锁包住有界 channel 的 send().await，否则一批快速回复会阻塞父 process 继续分发，连 shutdown 也无法启动。
- `deliver` 的 `Ok` 表示已投递或已接收到恢复缓冲，不表示任务执行完成。退出期 snapshot 保存失败沿用现有告警和返回行为，不默默丢弃内存中的 envelope，也不把本轮重构解释成新增持久交付保证；更强的存储失败策略另行设计。
- Exiting 缓存不绕过迟到回复 fencing、审批有效性或后台冷启动清理：前台可恢复消息照常恢复，已终止后台执行的消息仍按既有规则清理。

信任边界与校验归属：

- 文件声明进入 `AgentTeam::new` 时校验拓扑、名称与工具冲突，构建后的 Program 依赖这个保证。
- driver 负责纯批次结构预检，再解析未被拒绝调用的 schema；`invoke` 校验 caller 可调用目标、root 后台资格、当前执行、busy、生命周期和分组准入。批次结构检查与实时准入职责分开，不能通过逐项 busy 检查替代整批重复拒绝。
- 外部审批恢复沿用 Session 入口，匹配 pid、parent message、call id 及有效执行；迟到 Reply 匹配调用 envelope/execution。runtime 维护唯一运行准入规则，driver 维护批次预检并消费已路由消息。
- checkpoint/snapshot 恢复校验 program 可解析、身份与残留 execution 一致；已有 replay/fence 校验保持在恢复边界。不得为简化构造默认接受缺失或矛盾记录。

## Load-Bearing Decisions

1. **固定 pid 的 Process 替代可选 thread 的 Agent。** Program 是共享定义，driver 是执行器；driver 回收不销毁持久 memory。Process 逻辑上空闲可恢复，无须新增常驻 actor。
2. **process group 是执行期分组。** `ProcessGroupId` 用 `Foreground(TurnId)` / `Background(TaskId)` 表达，承接现有 ExecutionScope；不另造 PGID 注册服务。session 的“默认前台组”是每轮重新建立的逻辑位置，不是跨轮永久取消域。
3. **前台继承、后台独立。** 每次后台 subagent 启动创建一个组，同步后代继承；前台结束不取消其他后台组。相同 program 的 stateless 调用可生成多个 process/group，stateful 重叠调用仍拒绝。
4. **分组成员是 `(pid, invocation_id)`。** stateful process 以后可以在别的执行中加入别组。所有取消、迟到回复、回收和持久清理必须比较 execution 身份；旧组不得删除或终止同 pid 的新执行。旧组终止或清理未完成时按现有规则阻止复用。
5. **答案完成不等于资源全部结束。** 后台 subagent 回答后，其拥有的后台 shell 可能仍运行；保留 task 的终态结果与 subtree_active 的区别及停止入口。root 的普通后台 shell 保持原有 registry 行为，不为命名统一增加 agent group。
6. **错误清理强于回收优化。** checkpoint 失败先关闭组、撤销审批、取消工作和隔离成员；只有持久清理成功才允许复用。正常回复前的 driver 退休、call ledger 释放顺序保持不变。
7. **跨重启行为不扩展。** 冷启动清理未完成后台执行并记录 Interrupted；通知 receipt、完整结果读取去重和 fork/rewind 对 receipt 的规则不变。
8. **存储与协议保持原有形状。** `thread_id` 等字段仍是逻辑 process 身份的边界表示，避免纯命名 SQL 迁移；调整 Rust 类型及转换即可。若实现发现语义性字段变更确实必要，再修订设计，不手改生成的 schema.rs。

## Requirement Review

建议同步调整需求，均用于消除歧义，不扩大功能范围：

- 明确 Program 仅在同一 session 内复用，不能把它当作跨 session 全局单例。
- 将“默认前台分组”明确为按 turn 建立；成员归属 execution，idle process 不永久属于旧组。
- 将“subagent 启动以 spawn 理解”细化为新实例 spawn、已有 stateful 实例继续调用，避免与保留隐式复用规则矛盾。
- 区分后台答案完成与其拥有资源全部结束，要求旧组清理不能伤及复用 pid 的新执行。
- 明确保留 SQL/协议 thread 字段名称，也不强制 envelope/channel 改名为 pipe；本轮重点是状态和执行职责。
- 补充保留同批重复 stateful 调用全部拒绝，以及退出期间接收并保存有效在途消息的行为；分别由 driver 批次预检与 runtime 退出交付路径保证。

## Risks / Open Questions

- 最高风险是旧组清理误操作同 pid 的新 execution；第一步补足这个边界的行为验证，再迁移成员登记。
- 去除 driver 的 active_thread/suspended_thread 等变量时，可能误删审批及回复等待阶段；以固定 pid 加执行阶段表达，保留 replay 和 pending reply 测试。
- Program 工具及动态知识句柄存在共享状态，不能声称 Arc<Program> 即代表全部依赖只读；用双 session 注册表隔离与同 workspace 文件锁测试验证共享范围。
- `coda_execution` 名称覆盖 OS 进程和后台注册，但仍含 subagent 任务数据；本轮接受这条现有边界，不扩展成通用执行框架。
- 需用户审阅的设计取舍：按执行期分组、保留外部调用及字段名称；显式 spawn/pid API 仍是后续议题。

## Implementation Roadmap

- [x] [行为基线] 检查并补足同 pid 跨执行分组、旧清理/迟到回复、答案完成后 shell 仍存活，以及同批重复 stateful 调用全部拒绝的回归用例。
  - Purpose：先固定最容易因概念整理而改变的取消与复用边界。
  - Verification：有针对性的 runtime/background 测试；重复 stateful 项无模型调用或后台任务创建，混合前后台也全部拒绝；无冲突目标仍执行，同名 stateless 的原并发 barrier 用例继续通过。
- [x] [crate 命名] 将目录及 package 改为 coda_execution，同步 Cargo.lock、依赖、代码和当前架构文档。
  - Purpose：先消除 OS 执行 crate 与逻辑 process 的命名冲突，不搬迁职责。
  - Verification：workspace 构建、clippy、测试及 pg-tests 全目标编译。
- [x] [定义与状态] AgentTeam 构建 Arc<Program>；将 AgentState 和历史操作归入 Process；模型请求构造使用 Program 与实例 memory。
  - Purpose：工具按 session 构建，process memory 独立；先让现有 driver 使用新对象并保持可编译。
  - Verification：spec、消息视图、compaction、工具状态隔离和并发测试。
- [x] [路由与恢复] 引入 ProcessId，runtime 表按 pid 管理；固定 driver 身份，移除 Agent::for_thread 及多 thread 选择结构，更新 checkpoint 转换和服务端调用。
  - Purpose：运行时直接管理 process，完整保留恢复与实时快照能力。
  - Verification：审批、checkpoint、stale replay、orphaned reply、server hub 及 fork/rewind 测试；补充父 driver 已退出、子调用在 graceful shutdown 期间返回 Reply，消息进入持久 snapshot 且重开后正常消费的场景。覆盖退出切换时的 inbox/runtime 缓存收集、新调用被拒绝、Closed 后返回错误，以及 snapshot 保存失败保持现有返回和告警行为。
- [x] [执行分组] 整理 Scope 为执行期 ProcessGroupId，统一调用准入与成员身份检查；保留前后台完成、通知和资源清理差异。
  - Purpose：分组能直接解释取消行为，同时不引入新后台权限或实例选择方式。
  - Verification：后台 lifecycle/persistence/approval、清理 fencing、通知 receipt 与 shell 所有权测试。
- [x] [集成与文档] 清理过时命名和注释，更新 AGENTS.md 架构说明；审阅 prompts，只有模型所需规则实际变化时才调整。
  - Purpose：新维护者无需了解旧 Agent/thread 中间层即可理解执行模型。
  - Verification：最终运行 cargo clippy、cargo test、cargo check -p coda_server --features pg-tests --all-targets；存储行为变更时在确认可用的临时数据库运行 pg-tests，未运行则明确记录。若修改 web 代码，追加其 lint/test。

## Deviations from Design

- 预检沿用“返回重复目标名称、逐项拒绝”的纯函数形式，不新增 CallId 包装；invoke 保留现有错误文本，消息分发继续使用 SendCommandError。
- 前台取消继续使用 request_abort，后台取消明确命名为 stop_background_group，不加通用 stop_group 转发层。持久清理继续异步重试，完成性由现有状态查询表达；没有新增同步 CleanupError API。
- Envelope 保留用于首次启动及 snapshot 恢复的 program 名称元数据；在线 driver 固定 pid，Program 本身不参与收信。存储和事件中的 thread 字段仍保持原有格式。
- 旧组清理路径原先存在仅按 pid 删除的操作，本轮按已确认的 execution 身份契约补齐内存与 PostgreSQL fencing；尚未结束持久清理的组继续阻止会话维护操作。
- 已审阅默认 system prompt 和 templates 的委派、取消与恢复规则。模型可见操作未变化，无需修改提示词；架构说明已更新到 AGENTS.md。

## Implementation Validation

- 分支：`refactor/agent-process-model`；需求与设计初始提交：`52029be1`。
- 已通过 `cargo clippy`（无警告）、`cargo test`、`cargo check -p coda_server --features pg-tests --all-targets`。
- 已使用项目指定的本地 `coda_test` 数据库执行 `cargo test --features pg-tests`，47 个 PostgreSQL 存储测试全部通过；已有的一个 provider 测试保持 ignored。
- 新增验证覆盖独立 process 的 memory 隔离、同批 stateful 前后台重复调用全部拒绝、退出后的 Reply 恢复及单次消费、snapshot 写失败保留内存消息，以及旧组清理对新 invocation 的内存与数据库隔离。
- P1 背压回归：通过公开 Session API 一次提交 32 个立即完成的 stateless 子调用，验证所有回复被接收且 shutdown 有界返回；另以满 inbox 确定性验证 request_exit 不等待容量，以及退出后接收方关闭或容量可用时都只归档一次。该确定性用例在修复前因 request_exit 超时失败。
- 未修改 web 代码；本轮不涉及部署或远程推送，实现改动保留在工作区供审查。
