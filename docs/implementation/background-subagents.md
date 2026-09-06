# 后台 Subagent 设计

状态：实现及回归已完成；首版仅允许 root 启动后台 subagent。

## Problem

让 root 可以后台委派耗时工作，继续当前对话，并通过现有后台任务机制观察、审批和停止执行。需求见 [background-subagents.md](../requirement/background-subagents.md)。

## Scope

In：root 的同步/后台委派选择；stateful/stateless 执行隔离；后台执行中的同步委派；独立审批；完整最终答复及可靠通知；后台 shell 归属和取消；重连、重启及 session 操作的衔接。

Out：后台 reasoning/tool 流展示；重启恢复后台执行；改变权限策略；把任务复制到 fork。

## 已确认的边界

- 只有 session 的实际 root thread 可以后台委派自己可用的 subagent。根据调用者身份判断，不根据 agent 名称、配置中的 mode 或是否正在后台运行判断。
- 默认同步。所有 subagent 仍可按现有配置进行多层同步委派；后台 shell 保持现有能力。
- A（root）→ 后台 B → 同步 C：C 用原有 Reply 返回 B，B 处理后结束后台 task，以 notice 通知 A。通知始终交给直接调用方；首版中后台 subagent 的直接调用方必然是 root。
- Stateful 按父 thread + subagent 名称隔离；同一会话存在未结束的同步或后台调用时，新调用立即报错，不排队。
- Root 可以在后台 B 运行期间结束自己的 turn。聊天区停止只取消当前 root turn 及其同步调用；面板停止或 `task_kill` 才停止指定后台任务及其拥有的工作。
- 浏览器断线不停止任务，重连后可继续审批。服务重启后未结束的后台任务标记为 `Interrupted`，不恢复后台执行或审批。
- 后台审批只暂停对应执行；session 仍有任意未决审批时，拒绝新的用户消息和自动 root notice turn。其他已经运行的工作继续。

## Validation Findings

实现已完成以下专项验证，最终检查结果见文末。

| 核对点 | 实现及验证 |
| --- | --- |
| 执行隔离 | driver、inbox、恢复目标均按 thread 索引；同名 stateless 并发测试覆盖同步与后台调用，stateful 冲突立即拒绝 |
| Root 资格 | 工具参数与 runtime dispatch 双重校验；非 root 强传参数、同名非 root thread 均不能启动后台 subagent |
| 同步后代 | B → 同步 C 保留 Reply 路径；root 可先结束 turn，C 完成后 B 继续并提交完整结果 |
| 审批归属 | runtime 持有按 thread/批次隔离的审批集合；后台审批、root abort、定向移除和重复 call ID 已有回归测试 |
| Checkpoint 失败 | 注入 C 待审批保存失败和后续清理失败，验证 B 进入 Failed、无 Reply 等待或审批、无关任务继续；中止事务完成前保持 thread 隔离 |
| 冷启动与迟到写入 | 打开 session 前中止后台 checkpoint，PG 事务记录旧 invocation 的写入屏障；旧快照和 checkpoint 不能恢复已中止调用 |
| 结果及通知 | 完整结果重复读取、重开归档、未提交结果清理、通知容量背压均有测试；通知事务返回不确定时复用同一开场，收据保证一次入史 |
| Relay | 丢事件与缓冲溢出改用运行中快照；回归验证不会为重建客户端投影而关闭 runtime |

## 设计依据

| 决策 | 原因 | 取舍 |
| --- | --- | --- |
| 同一 runtime 按 thread 驱动 | 保留 stateful 会话，共享配置和审批策略，隔离各 thread 的运行状态 | 需要调整 driver 路由、调用占用和恢复索引 |
| 复用 `coda_process` 注册表，将 `BackgroundProcesses` 更名为 `BackgroundTasks` | 现有 future 接口可承载 subagent，归档及后台 shell 可继续共用 | 扩展任务类型和结果表示，保持该 crate 不依赖 `coda_agent` |
| 独立不可变结果文件和 session 级收据 | 完整保存答复、支持幂等读取，并保证 rewind 后不重复投递 | 增加一种输出形式和一张收据表 |

## Components

| 组件 | 职责 |
| --- | --- |
| `AgentRuntime` / driver | 校验 root 后台资格，按 thread 执行，管理调用占用、定向审批、scope 失败清理及 thread 隔离 |
| `coda_process::BackgroundTasks` | 统一管理任务身份、状态、结果归档、关联 shell、停止和待交付通知 |
| `ToolCallContext` | 把所属后台 task ID 传给工具，使同步后代启动的 shell 正确归属 |
| `SessionHub` | 管理 root 输入/notice 准入、审批投影、任务推送、重连及 session 生命周期 |
| `SessionStorage` / PostgreSQL | 保存 checkpoint、执行归属和通知收据，事务清理中止 scope，并拒绝旧执行的迟到写入 |
| `coda_web` | 展示后台任务和关联 shell、最终答复及定向审批 |

调用占用归 runtime，任务及输出归注册表，消息和收据事务归 storage；不增加只转发这些接口的协调层。

## Data Model

### 执行和任务身份

- `ThreadId` 标识会话；`MessageOrigin(parent_message_id, call_id)` 标识一次委派；`TaskId` 标识一次后台任务。Stateful 和 stateless 的 thread 派生规则保持不变。
- 每个活跃 thread 一个 driver，独占 `AgentState`。前台同步调用完成后，先持久化最终 checkpoint、移除 driver 注册和执行记录，再交接 Reply 并退出；stateful 下次调用从 checkpoint 恢复历史。后台成员仍由所属 scope 统一回收。工具、模型配置和 prompt 知识句柄可共享，消息、当前 turn 和工具状态不能跨 thread 共享。
- `ExecutionScope = Foreground { turn_id } | Background { task_id }`。同步后代继承 scope；只有 root 的后台分支创建新的后台 scope。
- `CompletionTarget = RootTurn | Caller(ReplyTarget) | BackgroundTask(TaskId)`。同步 C 完成仍返回 B；后台 B 完成直接交给注册表，不对已返回启动结果的 root tool call 再补 Reply。
- `StoredExecution` 保存当前调用身份、scope 和 completion target，终态后清空。它替换独立的 `reply_target` 字段；runtime 快照、inbox 和恢复目标改用 thread ID 索引，并记录 agent 名称。
- Runtime 记录每个后台 scope 参与过的 thread 及调用身份，覆盖 B、已登记但尚未启动的同步后代和已执行的各分支。失败时按这些 thread 建立隔离记录，包含 task ID、失败原因和清理进度；不能按 agent 名称隔离其他 scope 的同名调用。
- `HistoryEntry.turn_id` 沿用触发委派的 root turn，支持 fork/rewind 切分历史；后台执行生命周期由 `ExecutionScope` 管理。

### 任务记录及形状

```rust
enum TaskKind {
    Shell { command: String },
    Subagent { agent_name: String },
}

struct TaskMeta {
    kind: TaskKind,
    description: String,
    parent_task_id: Option<TaskId>,
    origin: TaskOrigin, // 调用者 thread、MessageOrigin、实际 agent 路径
}

enum TaskStatus {
    Running,
    WaitingApproval,
    Cancelling,
    Completed { at: Timestamp },                // subagent
    Exited { code: Option<i32>, at: Timestamp }, // shell
    Killed { at: Timestamp },
    Failed { message: String, at: Timestamp },
    Interrupted { at: Timestamp },
}
```

时间类型沿用 `jiff::Timestamp`。前三种是未终结状态，均占用现有 16 个后台任务名额并阻止 hub 释放。Subagent 同步等待 C 时继续使用现有 reply 等待机制，任务概要保持 `Running`；等待本身不额外调用 LLM。

所有后台 subagent 都是任务列表的顶层节点，`parent_task_id = None`。后台 B 及其任意同步后代启动的 shell 以 B 的 task ID 为 parent；前台执行启动的 shell 没有后台父 task。注册表表达顶层任务和关联 shell；同步调用路径可多层，用于审批说明和取消。

B 正常完成后，它启动的后台 shell 可继续运行；只要关联 shell 存活就保留 B 的身份和停止入口。再次调用同一 stateful thread 不改变旧 shell 的归属。完成通知统一交给直接调用方 root。

### 持久化和共享状态

- 将严格解析的纯值类型 `TaskId` 移到 `coda_core`，供 runtime、工具上下文和注册表使用。
- 归档 manifest 增加 kind、origin、parent、结果位置及 `Pending/Delivered` 通知标记，提升版本。不引入兼容层；旧格式不可读时明确报告。
- 后台 scope 的失败归档保留 `cleanup_pending` 及成员调用身份。任务已 `Failed` 但数据库尚未清理是合法状态；该标记在 storage 中止事务成功后才清除，并在此之前保留元数据。任务名额与 thread 隔离分别管理，不能用一个长期 `Running` 的任务代替隔离。
- Shell 保留输出环及增量游标；subagent 用不可变结果文件，只保存完整最终答复或明确错误，读取不消费结果。完整执行历史仍在 thread checkpoint。
- 结果计入现有 64 MiB session 归档配额；待交付结果不得淘汰。容量不足时拒绝新任务，结果保存失败则记 `Failed`，不能截断后宣称 `Completed`。已交付结果沿用有限保留策略，回收后面板明确显示过期。
- 增加 `task_notice_receipts(workspace_id, session_id, task_id, message_id)`，主键为前三项，复合外键指向 `sessions` 并级联删除。收据与 root notice 入史同事务写入，不随 rewind 删除，不复制到 fork。
- Migration 将 checkpoint 的 `reply_target` 改为 typed JSONB `active_execution`；调整 storage 和序列化，`schema.rs` 由 Diesel 生成。
- 共享可变状态是 runtime 的占用/审批集合、注册表的任务状态/关联 shell，以及现有 permission mode cell。短锁内不得等待 driver、工具退出或数据库操作。

## Interfaces

### 模型及客户端边界

```ts
// 只有实际 root thread 的工具定义包含此可选参数。
agent__<name>({ task: string, run_in_background?: boolean })
// 同步返回既有 ToolOutput；后台返回含 task_id 的启动结果。
// 参数非法、非 root 后台调用、stateful 忙碌、thread 尚待中止清理或容量不足均返回工具错误。

task_output({ id: string })
// Shell 保持增量读取；subagent 返回状态和完整最终答复，重读幂等。

task_kill({ id: string })
// 停止 task；若为 B，同时停止其同步调用及关联后台 shell，重复停止幂等。

get_task_result({ workspace_id, session_id, task_id })
// 面板按需获取最终答复，不推进工具游标；未知、过期、I/O 错误明确区分。

resume({ workspace_id, session_id, agent_name, thread_id, decision: {
  parent_message_id, resolutions
}, allow_patterns?: Array<[call_id: string, pattern: string]> })
// JSON-RPC request，返回 { accepted: boolean }。
// 只处理该 thread 的指定审批批次；过期决议返回 accepted: false。
// “总是允许”规则随决议提交，只有有效且被接受的 shell 批准才写入配置。
```

**信任边界**：工具定义按实际调用 thread 生成，只有 root 且注册表可用才暴露后台参数；runtime 在 dispatch 入口再次校验资格、参数类型和该调用者可用的 subagent。非 root 强行传入 `true` 必须报错，不能只靠 prompt 或 schema 限制，也不能降级为同步。Root 的判定使用 `is_root_thread`，同名 agent 的非 root thread 不获得资格。下游只接收已验证调用，不重复推断调用者权限。

任务身份、scope 和 parent 由系统生成，不能由模型填写；raw task ID 在工具/RPC 边界严格解析。后台注册表不可用时，root 强行传入 `true` 也明确报错。Programmatic host bridge 只透传工具 scope，不扩大可调用工具集合或提供后台委派旁路。

RPC 验证 workspace/session 和最新 attachment；runtime 统一核验审批批次、call ID、resolution 及调用仍可恢复。过期审批也不能写入“总是允许”规则。`PendingApproval` 增加所属 task ID 和完整 agent 路径；server/frontend 的审批、草稿和 in-flight 标记按 `(thread_id, parent_message_id, call_id)` 寻址。

`TaskSummaryWire` 增加 kind、parent task、typed status、`subtree_active` 和 `result_available`；摘要不带完整答复。面板按顶层任务/关联 shell 排列，后台 B 的同步 C 不增加独立任务行；审批仍标出实际执行者 C。

### 内部行为接口

```rust
// 接受一次已验证委派；后台重放同一 origin 返回已有 task ID，不重复启动。
async fn dispatch_subagent(&self, call: ValidatedSubagentCall)
    -> Result<DispatchOutcome, DispatchError>;

// 登记后台 future；parent 只允许关联到当前后台 subagent scope。
// 父 scope 已取消或容量不足时拒绝，不留下新执行。
async fn spawn_with<F, Fut>(&self, identity: TaskIdentity, meta: TaskMeta, work: F)
    -> Result<TaskId, SpawnError>
where F: FnOnce(TaskCtx) -> Fut, Fut: Future<Output = TaskExit> + Send + 'static;

// 停止任务及所拥有的执行，等待审批撤销、driver 和进程组清理。
async fn kill(&self, id: &TaskId) -> Result<Option<KillOutcome>, TaskAccessError>;

// 立即封闭失败 scope，并由 runtime 接管其清理；重复报告幂等。
// 报告者不等待自身退出，错误事件的订阅者不负责驱动清理。
fn checkpoint_failed(&self, execution: ExecutionIdentity, error: String);

// 原子清理指定 scope 的 checkpoint 和排队消息，拒绝旧执行再写回。
// 仅在中止状态持久化后返回成功；失败时这些 thread 仍不可复用。
async fn abort_scope(&self, scope: ScopeAbort) -> Result<CleanupReceipt, StorageError>;

// 同事务保存 root notice 开场 checkpoint、可恢复执行状态和交付收据。
// 重试返回 AlreadyAccepted；失败不留下半条消息或收据。
async fn accept_task_notice(&self, opening: NoticeOpening)
    -> Result<NoticeAcceptance, StorageError>;

// 获取与后续事件衔接的 root 状态和全量审批，不停止运行中的任务。
async fn live_snapshot(&self) -> Result<SessionLiveSnapshot, SnapshotError>;
```

`TaskExit` 增加 subagent 完成结果，正常完成由 driver 在最终 checkpoint 成功后提交；scope 存储失败由 runtime 在执行清理完成后提交失败结果。两者共用注册表的一次终态提交规则，不能从普通 LLM 广播猜测完成，也不能让失败路径等待正常 Reply。`ToolCallContext` 增加所属后台 task ID，同步调用及 programmatic host bridge 透传它，使 shell 正确归属。

## Load-Bearing Decisions

### 1. Root turn 与后台执行独立，同步内部流程保留

后台 dispatch 先检查同一 origin 是否已登记，新调用才原子尝试占用目标 thread 并登记任务。后台 task ID 由 session 和完整 `MessageOrigin` 稳定派生，归档持久保存 origin；重复执行父工具批次不会重启任务。登记失败释放占用，接受后的启动失败通过任务终态报告。

Root 立即收到启动结果，不增加 `pending_replies`；后台 B 的同步 C 仍使用原 caller/reply 路径。占用贯穿生成、同步 reply 等待、审批及取消清理；正常路径在最终 checkpoint、执行退出和后台终态提交完成后释放。存储失败则转为 thread 隔离，直到中止清理成功持久化才允许复用。同批次重复 stateful 调用沿用现有拒绝行为，跨批次冲突拒绝后来者。

即使只有 root 能后台调用，root 仍能启动多个同名 stateless B，不同后台 B 也可能同步调用同名 C，因此仍需按 thread 驱动、按 scope 定向取消，不能保留按 agent 名称串行运行的共享状态。

`TurnGate` 只记录 root 前台 turn。后台 scope 的 LLM/tool 内容流在进入 session relay 前过滤，审批、任务状态和持久化错误通过控制事件展示；存储失败的处置由 runtime 直接驱动。B 的同步后代都属于 B 的后台 scope，不把它们的结束误判为 root 结束。

### 2. 审批集合与 root 状态分离

Runtime 按 `(thread_id, parent_message_id)` 持有权威审批集合，checkpoint 保存成功后发布 added/removed 事件，发布及恢复都与 scope 的取消状态串行判定。Hub/front-end 接收定向变更，重连获取完整集合。`approval_removed` 和 `background_error` 的实际序列化 JSON 与前端共用样本验证；快照按仍存在的 thread、审批批次及 call ID 保留草稿，仅清理失效项。

后台 B 或其同步后代等待审批，B 的 task 显示 `WaitingApproval`；多个同步分支产生多个批次时，处理一项不能清除其余项。批准/拒绝只恢复对应 thread，其他 task 的运行不受影响。拒绝是工具结果，subagent 可以继续生成，不直接把 task 标为失败。

后台审批新增/恢复不改变 root 的运行状态；root 结束不清空后台审批。输入准入在 runtime 内与新增审批有确定先后：注册 pending 后的新 root 输入拒绝，此前接受的 turn 可继续。UI 禁用和 hub 校验都是该规则的投影。

### 3. 停止一个后台执行及其关联 shell

后台 B 拥有一个取消 scope，其全部同步后代继承它；这些调用启动的后台 shell 登记到 B 的 task ID。Root turn 的 token 不作为后台 B 的父 token。

停止 B 时，注册表原子标记 scope 取消中并封闭其新任务登记，再通知 driver 和关联 shell。与 spawn 的竞争只有“先登记并被覆盖”或“登记被拒绝”。Runtime 撤销该 scope 的审批、写入中止工具结果，工具按既有取消宽限清理，shell 结束整个进程组。不能只丢弃 future 就宣布成功。

B 已正常完成而关联 shell 仍活跃时，保留 B 的终态并停止这些 shell；`subtree_active` 保留面板停止入口。停止单个 shell 不向上取消 B。若 B 或其同步后代调用 `task_kill(B)`，只提交取消请求并返回，避免等待自身退出；UI/外部调用可等待清理屏障。

Hub 不持 entry 锁等待依赖事件转发的清理：验证 attachment、提交请求后释放锁等待，再核对最新 attachment；等待期间不得将旧连接的结果发送给新连接。每个任务终态只提交一次，完成与停止竞争由提交点决定。

### 4. Root 接收完整结果，完成事实只入史一次

正常完成时，后台 B 的最终 checkpoint 成功后保存完整结果及 manifest 的待交付标记，再发布终态摘要。存储失败时，通知记录实际失败原因和历史尚待清理的事实，不把未持久化的答复作为成功结果。通知队列只保存身份；未交付结果不采用 shell 的 4 KiB 截断、overflow 汇总或读取即回收。每个后台 B 使用一条身份稳定的 `TaskNoticeMessage`，直接交给调用者 root；首版不批量合并 subagent 通知。

注册任务时预留待通知名额，沿用现有 full notice 数量级作为容量上限；耗尽时拒绝新后台 subagent，已接受任务仍能完成。不为维持队列上限而丢弃最终答复。

投递顺序：取得 root 空闲槽且确认无待审批 → storage 将 notice 开场 checkpoint、可恢复前台执行和收据同事务提交 → 驱动同一 root 开场 → hub 尝试 ack 归档标记。明确未提交时释放槽、保留记录；事务结果尚不能确认时保留开场和槽，重试查询收据后继续。ack 失败重试但不阻止已提交处理。已有收据只补 ack，不再次追加 notice 或新建 turn；若事务成功但返回结果丢失，接管已持久化的同一前台执行。

这里“一次”指一项完成事实最多追加一条 notice；root 已主动完整读取时无需追加。不承诺崩溃恢复期间的 LLM 请求或外部工具副作用恰好执行一次。Root 接收方随 session 存活。

主动读取也可以完成交付：`task_output` 完整读到终态结果时，将类型化 task ID 随工具结果记录。只有实际 root thread 的成功、非中止结果才在 checkpoint 事务内写入同一 `task_notice_receipts` 表，`message_id` 指向该 ToolMessage。收据既可对应 notice，也可对应完整读取；rewind 不删除，fork 不复制。shell 待投递队列按收据过滤，subagent 仍走已有收据检查和归档 ack，已经确认的完成事实不再启动额外 turn。

读取侧不直接撤销通知：Running、尚有未读分页、输出缺失/过期、I/O 失败和非 root 读取均不确认交付。面板读取不产生工具结果收据。checkpoint 失败或事务返回不确定时，以数据库内的收据为准；不能解析输出文本猜测任务身份或终态。

shell 归档以 `output_lost` 保存读取过程中是否曾跳过被覆盖的 stdout/stderr 字节，与游标在同一次 manifest 提交中持久化；后续分页、进入终态或重新打开归档都不能清除该标记。`complete` 必须同时满足终态、无剩余分页和从未丢失输出。manifest 格式升为 v3，旧格式按既有版本校验拒绝打开。

`Consumed` 表示输出已读完，可能在 Running 期间读完、退出时自动回收。此时 root 再读取终态也应确认交付，只要 `output_lost` 为 false；重复终态读取共用同一 task 收据。`Expired` 表示未读输出已过期，不能确认交付。

### 5. 任意 checkpoint 失败均结束所属后台 scope

**责任和触发范围。** B 或任意同步后代保存开场消息、审批、工具执行、等待 Reply、结束结果等 checkpoint 失败时，driver 直接调用 `AgentRuntime::checkpoint_failed`。Runtime 根据可信执行身份找到后台 scope，原子封闭该 scope 的新委派、工具启动和审批恢复，并将全部成员 thread 标为隔离。控制事件用于更新 hub/UI；后台故障不走关闭整个 session 的路径，无关 scope 不被连带取消。

**先结束执行，再修复持久状态。** Runtime 自己持有清理工作，不要求报错的 C 等待自身退出，也不依赖 B 再收到 C 的正常 Reply。具体顺序如下：

1. 撤销该 scope 的全部未决审批，向 B 及其同步后代发送专门的失败控制信号；该信号可唤醒正在等待 Reply 的 driver，结束其消息等待并进入退出清理，禁止继续生成。清理覆盖关联后台 shell，遵守工具退出宽限及进程组清理规则。
2. 确认 scope 内执行停止后，runtime 统一结清该 scope 的 `CallLedger` 义务，移除 `pending_replies`、reply target 和排队的调用/恢复消息。迟到 Reply 依据调用身份丢弃，不能重复结清或进入后续调用。这是终止整个 scope，不是假造 C 的成功回复让 B 继续运行。
3. 注册表提交 B 的 `Failed` 终态和一次失败通知，释放后台运行名额；保留所有成员 thread 的隔离。该终态只证明执行已结束，不证明 checkpoint 已修复。数据库持续不可写时，task 仍可结束，不等待数据库恢复；通知入史按正常重试规则等待存储可用。
4. Runtime 调用 `SessionStorage::abort_scope`：核对成员调用身份，在同一事务中清理这些 thread 的审批、待执行工具、Reply 等待、`active_execution` 及 runtime 快照中的排队消息。对已持久化但没有结果的工具调用追加明确的中止结果；已持久化结果保留，不补写未成功保存的正常答复或工具状态。事务提交成功才得到清理收据。
5. 中止事务成功后重新加载已清理 checkpoint，再清除归档的 `cleanup_pending`；确认任务已终态后解除该 scope 成员的隔离。任一步失败都保持隔离，后续调用返回明确的“等待中止清理”错误；读取数据库成功、重新连接或 task 已终态都不足以解除隔离。

**写入和重启约束。** 同一 thread 的 checkpoint 写入与中止事务必须串行并核验执行身份；中止清理成功后，旧 driver 的迟到写入必须被拒绝。快照写入也不能重新引入已关闭 scope 的排队消息。存储返回结果不确定时，重新读取持久状态核对后幂等清理，不能假定失败的事务一定没有提交。

失败归档记录成员身份和清理标记；写标记失败仍保持运行时隔离并报告归档错误。冷启动无论 manifest 是否完整、task 是否已终态，都先扫描 checkpoint/快照中的后台执行归属并执行中止清理，再决定哪些 thread 可复用；后台消息一律不重放。这样在失败处理或清理事务中途重启也不会执行旧工具。清理失败只保留对应 thread 的隔离；若恢复时无法读取必要的归属信息，则 session 暂不开放，不能猜测安全状态。

### 6. 重连保活、重启清理与 session 操作

活跃或等待审批的后台 B 始终保活 hub entry。浏览器重连继续附着原 runtime。纯 relay 丢事件或溢出使用 `live_snapshot` 重建投影，不能通过关闭 runtime 恢复连接；未完成流式文本可随后由完整消息补齐，审批和任务状态必须完整。

冷启动先重开归档，将非终态后台任务改为 `Interrupted`，保留已记录的失败终态；按第 5 节清理所有遗留后台执行归属和未完成的中止记录，不重放后台 envelope。未清理的审批不得作为可操作项恢复，thread 在中止状态成功持久化前保持隔离；Interrupted 完成事实仍通知 root。前台及已接受的 root notice 执行沿用恢复路径。

关闭 session 先封闭任务登记，取消并等待后台 scope 和关联进程清理，再结束 runtime、monitor 和归档锁。不能先退出 driver 再等待依赖 driver 结果的后台 future。异常重启产生 `Interrupted`；显式停止产生 `Killed`。

有活跃后台 subagent 或尚未持久化完成的 scope 中止清理时，普通对话及无关 scope 可继续，但 SetModel、手动 compact、fork、rewind 返回 `NotIdle`：这些操作会重建 runtime 或读取/修改其共享历史。Permission mode 仍可随时更新，仅后台 shell 运行时保持原有规则。后台 B 全部结束且中止清理完成后，fork/rewind 按来源 root turn 处理历史；fork 不复制任务、归档或收据，历史中的旧 task ID 不能控制原 session。

## Risks / Open Questions

没有待补充的范围问题，以下是实现时必须验证的风险。

- 最大风险仍是 driver 按 thread 隔离后，前台/后台审批、reply、取消和 checkpoint 是否正确配对。先做 fake provider 的最小并发切片，不先做 UI。
- Root-only 必须由实际 thread 身份强制执行，覆盖隐藏字段、历史重放和同名非 root thread；不能依赖模型遵守描述。
- 覆盖 stop/resume/spawn 竞争和 self-kill，证明没有死锁、迟到审批或遗留进程。
- 对任意层级的审批/工具状态 checkpoint 注入失败，验证 task 终态与 thread 解除隔离分别成立；清理再次写入失败、事务结果不确定和中途重启都不得重放旧调用。
- 对结果保存后/notice 入史前、入史后/archive ack 前注入故障，验证不丢、不重。超长答复按需读取；无法完整保存时失败，模型上下文超限时保留已入史 notice 并明确报告，不能静默裁剪。

## Implementation Roadmap

Runtime、注册表、持久化、Hub 和 Web 已接通。下列阶段的完成状态以对应专项回归为准。

1. [x] **[风险验证 / runtime]** 限制 root 后台资格，按 thread 隔离 driver，建立后台 B + 同步 C 的最小切片。
   目的：证明 root 可继续对话，内部同步调用保持正确。
   验证：非 root 后台请求无副作用地报错；两个同名 stateless 并行；stateful 同步/后台冲突立即报错；C 回复 B、B 通知 root；C 保存待审批 checkpoint 失败时，B 进入 `Failed`、释放任务名额，无残留 Reply 等待或审批，无关后台任务继续。
2. [x] **[注册表 / 工具]** 增加 task kind、agent 结果归档、scope 和关联 shell，接入读取及停止。
   目的：复用任务体验，同时保证整个后台执行可停止。
   验证：停止 B 覆盖多层同步后代及 shell、无关任务继续；B 已结束后的 shell 可停止；spawn/kill 竞争、自身停止、容量和 I/O 失败正确处理。
3. [x] **[审批 / relay]** 拆开 root 状态和全量审批集合，接入定向变更及重连快照。
   目的：后台审批独立处理，输入准入前后端一致。
   验证：多个 task、同名 agent、call ID 复用不串批次；root 完成不清空后台审批；重连及 relay 缺口不取消任务；权限策略回归。
4. [x] **[持久化 / 通知]** 迁移执行元数据和快照索引，增加 scope 中止事务、隔离恢复、root notice 收据事务、归档 ack 和后台冷启动清理。
   目的：保持一次入史，保证失败 scope 清理完成后才可复用，重启不恢复后台执行。
   验证：C 保存审批或工具状态失败、清理再次失败时，B 已终态而成员 thread 仍隔离；清理成功后才可复用。覆盖迟到写入、事务结果不确定、清理完成前重启，恢复后不重放 C 的旧调用；无关同名 thread 不受影响。Interrupted/root notice、重复投递、rewind/fork 及 storage pg-tests 回归。
5. [x] **[Hub / Web]** 接通任务摘要、最终答复、审批归属、关闭顺序及 session 操作准入。
   目的：让用户完整观察和控制首版后台任务。
   验证：后台 B 顶层展示、关联 shell 归属正确；无连接时仍运行及通知；root abort 保留后台；删除/shutdown 无遗留执行；操作限制明确且可解除。
6. [x] **[验收 / 文档]** 完成需求场景回归，更新工具说明、协议和架构文档。
   目的：确认首版范围、默认同步行为及错误路径完整。
   验证：`cargo clippy`、`cargo test`、`cargo check -p coda_server --features pg-tests --all-targets`；一次性 PostgreSQL 的 feature-enabled storage tests；`pnpm --filter coda-web lint`、`pnpm --filter coda-web test`。


## 最终验证结果

2026-09-05 完成：

- `cargo clippy --all-targets`：通过，无警告。
- `cargo test`：workspace 全量测试通过；原有一项标为 ignored 的测试保持原配置。
- `cargo check -p coda_server --features pg-tests --all-targets`：通过。
- `cargo test -p coda_server --features pg-tests --test storage_pg`：使用独立临时 PostgreSQL 数据库，44 项通过，包含 scope 中止、迟到写入、通知事务回滚、rewind/fork 收据隔离。
- `pnpm --filter coda-web lint`、`pnpm --filter coda-web typecheck`：通过。
- `pnpm --filter coda-web test`：17 个测试文件、135 项通过。
- `git diff --check`：通过。

数据库迁移已加入 `20260905000000_background_subagents`，`schema.rs` 由 Diesel 生成。迁移由服务启动时的现有流程执行。


### Reviewer 问题修复验证

- 修复两个控制事件的 JSON 类型名，并以同一份 JSON 样本连接 Rust 实际序列化测试与前端 reducer 测试。
- 覆盖 root 完成/停止后紧接快照的审批草稿保留，以及已结束 call、替换批次的草稿清理。
- 连续 16 轮、32 次 stateless 同步调用后仅保留 root driver 与执行记录，完成的 JoinSet 项也及时回收；原 stateful 连续调用历史测试验证 driver 重建不会丢失会话。

本轮修复后重新通过 `cargo clippy --all-targets`、`cargo test`、`cargo check -p coda_server --features pg-tests --all-targets`，以及前端 lint、typecheck 和 135 项测试。真实进程崩溃与数据库联动、浏览器 E2E 本轮未验证。

### 主动读取后的通知去重

- 只读核查 `cowork/d0b000fc-192f-4997-a30f-491a56c96900`：user message `9830d361-769c-4748-b0ae-4d1824c0931c` 后，两次 shell 终态读取分别在 seq 24、32，root 总结在 seq 33；seq 34 又合并同两项完成事实，导致 seq 35 重复答复。
- [x] 完整终态读取通过工具上下文携带 task 身份，根线程成功结果与收据同事务保存。
- [x] Hub 在 shell 批次投递前剔除已有收据的任务，保留未读项；subagent 复用已存在的收据和 ack 路径。
- [x] 验证完整/部分读取、root/非 root、混合通知、checkpoint 回滚、重开及 rewind/fork；业务库只读，数据库测试使用单独临时库。

本轮通过 `cargo clippy --all-targets`、`cargo test`、`cargo check -p coda_server --features pg-tests --all-targets`，以及独立临时 PostgreSQL 库中的全部 46 项存储测试。Hub 后台专项 13 项和 task 工具专项 4 项通过，覆盖正常结束、被 kill、已读与未读混合、subagent 归档确认等路径。

- [x] 审阅补充：跨页累积输出丢失标记，stdout/stderr 任一丢失后，后续分页、归档重开和 Running → 终态都不确认完整交付。回归测试先复现最后一页误判，再验证修复及 task_output 不记录收据。

该补充通过 `cargo clippy --all-targets`、`cargo test`、`cargo check -p coda_server --features pg-tests --all-targets`、task 工具专项 5 项及 `git diff --check`；未重跑真实 PostgreSQL 测试。

- [x] 实测补充：`cowork/4a44f5a1-a9c3-4efc-9541-d0d97be9af0f` 在 seq 20 已确认自然退出，但输出在 Running 期间已读完、退出时变为 Consumed，因此漏记收据并在 seq 30 重复通知。Consumed 且无累计丢失的终态读取现可记录收据。新增 Hub 测试先复现，再确认运行中读取、退出回收、终态读取、收据保存及通知过滤完整路径通过。

该补充通过 Hub 后台专项 14 项、task 工具专项 5 项、`cargo clippy --all-targets`、`cargo test`、`cargo check -p coda_server --features pg-tests --all-targets` 和格式检查；业务库只读，未重跑真实 PostgreSQL 测试，未再次变更归档格式。
