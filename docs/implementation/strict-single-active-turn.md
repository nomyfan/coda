# 每个 Session 只允许一个活跃 Turn

## Problem

当前 runtime 允许新的根 `Task` 在上一轮尚未结束时入队，并用“顶替旧轮”的协议处理。Web 在审批挂起后也会重新开放发送入口，但产品预期是用户必须先处理审批或中止当前轮，再开始下一轮。多轮并存既暴露了错误交互，也让取消、恢复和信箱仲裁承担了不需要的复杂度。

## Scope

**In**

- 一个 session 同时至多有一个尚未最终结束的根 turn。
- 第二个 `task` JSON-RPC request 返回明确错误，不进入 agent 信箱。
- 等待审批仍算 active；Web 在此期间禁止发送新任务。
- 将 runtime 的有序 active-turn 集合收窄为单个可选 active turn。
- 删除只为多个根 turn 排队或顶替服务的状态、分支、测试与注释。

**Out**

- 不改变 Abort 的递归收场：根仍要等已派发的 sub-agent 持久化并回话。
- 不删除 `unanswered` 差事账；它保证收场不会越过仍在写入的 sub-agent。
- 不承诺删除全部 deferred 信箱仲裁。同一 turn 内仍可能有多个 sub-agent `ToolCall` 到达同一个 agent，只有确认仅服务根任务顶替的部分才删除。
- 不改变 `TurnId` 事件标记和 Hub 的幂等结算；同一轮会先 `Suspended`、后最终结束，两次 settle 仍需按 id 区分。

## Assumptions

- 新 turn 只能由发给 root agent 的 `EnvelopeBody::Task` 创建；`ToolCall`、`Reply`、`Resume` 都属于已有 turn。
- 用户想更换任务时，先 Abort，等旧轮最终 settle，再发送新任务。
- 持久化格式可以破坏性变更，不为旧版产生的多-turn runtime snapshot 增加兼容逻辑。
- 崩溃后若 checkpoint 留着待回复调用，但当前进程没有任何 active thread 或回放信封能成为 producer，该旧轮不会被登记为 active；新任务仍可进入并把这些死调用记为 Aborted。这保留了 [`../requirement/persist-before-visible.md`](../requirement/persist-before-visible.md) 定义的恢复出口。

## Validation Findings

- Web 的 `task` 已是 JSON-RPC request，成功响应携带服务端生成的 `message_id`，因此拒绝可以沿现有 request/error 通道返回。
- `SessionHub::handle_task` 在 per-session entry lock 内调用 `Session::send`，多个客户端请求会串行经过这里；runtime 仍应作为最终准入点，覆盖 `Session` 的其他调用方。
- `Suspended` 会让 Web 把 `running` 设为 `false`，而 Composer 当前不看 `approvals`，所以审批期间可以发送是可复现的现有行为。
- runtime 在 `Suspended` 和等待 sub-agent 回话时都不会关闭 turn；因此同一份 active-turn 状态足以拒绝这两种情况下的新 `Task`。
- 当前 Web 的失败回滚会无条件把 `session.running` 设为 `false`。第二个请求被拒时，这会错误清掉第一轮的运行状态，需要随协议一起修正。
- `AgentLoop::run` 在读取 checkpoint 时可以在任何正常收场逻辑之前返回 `Err`；root 的错误分支目前既不发失败事件也不释放 active turn，因此 session 会永久锁死。
- runtime 发布最终事件后就释放准入槽，但 Hub 异步消费该事件。新 Task 可能在两者之间进入；旧事件结算时不能无条件把 `turn_running` 清成 `false`。

## Alternatives Considered

### 保留顶替，只修 Web

只在审批期间禁用 Composer 可以修正眼前交互，但直接调用 JSON-RPC、请求竞态和 `Session` 的其他调用方仍能创建多个 turn，runtime 还得永久维护顶替协议。这不能兑现“严格只能有一个 active turn”。

### 在 Hub 拒绝，不改 runtime

Hub 的 entry lock 足以保护 WebSocket 入口，但 `Session::send` 仍公开表达“随时可提交”，内部测试或其他宿主可以绕过不变量。把一致性责任留在调用者也无法真正简化 active-turn 数据模型。

### Runtime strict single-flight（选择）

runtime 在 `Task` 入队前原子登记 active turn；已有不同 turn 时返回领域错误。Hub 只负责把错误翻译成 JSON-RPC，Web 只负责提前表达 busy。代价是 runtime 的 `send` 新增一个可预期失败，但它把规则放在唯一不会被绕过的位置。

### Loop 错误后保留 runtime gate，等待 Hub 重同步

只发 `PersistFailed` 而不释放 active turn，可以阻止失败事件尚未被 Hub 消费时有新任务进入；Hub 重同步销毁 runtime 后，准入槽自然消失。但 `Session` 的其他调用方没有 Hub 帮它重建，失败 runtime 会永久拒绝后续任务，也正是本次 review 发现的锁死。选择显式释放，并让 Hub 在自己的事件账目结算前保持一道保守的入口检查。

## Interfaces

```rust
pub enum SendCommandError {
    TurnAlreadyActive,
    AgentNotFound,
    ChannelClosed,
}
```

`Session::send` 在已有 active turn 时返回 `TurnAlreadyActive`；被拒任务没有任何入队、持久化或事件副作用。

JSON-RPC `task` 将该错误映射为现有的 `SESSION_NOT_IDLE (-32006)`：

```json
{
  "code": -32006,
  "message": "the session already has an active turn"
}
```

## Data Model

runtime 只保存一个内存态：

```rust
struct ActiveTurn {
    id: TurnId,
    cancelled: bool,
}

turn: Arc<std::sync::Mutex<Option<ActiveTurn>>>
```

它仍然不单独持久化。bootstrap 在启动任何 agent 前，从 snapshot 的 active threads 与待回放信封恢复 `TurnId`；同一个 id 的多条证据幂等收敛。出现不同 id 表示 snapshot 违反 single-flight 契约，打开 session 失败，而不是任意挑一个继续。

Hub 的 `unsettled_user_messages` 同样至多一个，可以收窄为 `Option<(TurnId, Message)>`。它只负责把尚未折入快照的用户消息与事件关联；审批时该值已经被折叠为空，但 runtime 的 active turn 仍在，因此它不是任务准入依据。

## Load-Bearing Decisions

1. **准入在 runtime，不在 Web 或 Hub。** UI 状态和连接层 bookkeeping 都不是 session 执行不变量的可靠来源。
2. **审批挂起仍是 active。** `Suspended` 是 UI/快照的 settle 信号，不是 turn 的最终结束；只能 Resume、Reject 或 Abort，不能提交新任务。
3. **正常路径只有最终且已持久化的根结束事件关闭 active turn。** `Suspended`、等待 Reply、checkpoint 写入失败都不释放准入门。root 在进入 loop 时连 checkpoint 都无法读取是例外：它不可能自行收场，必须发 `PersistFailed` 后释放对应 turn，让 Hub 重同步、让其他 `Session` 调用方可以恢复。
4. **重启恢复多个不同 TurnId 时失败。** 项目允许 persisted-data breaking change；静默挑选会隐藏状态损坏并可能运行错误任务。
5. **崩溃后没有 producer 的旧 checkpoint 不占 active turn。** 是否 active 仍由“本进程真正会恢复的工作”推导，避免永久锁死 requirement 已定义的恢复出口。
6. **Hub 保留一道保守的次级准入检查。** `turn_running`、pending approval 或 unsettled user message 任一存在时，Hub 先返回 `NotIdle`；runtime 仍是最终权威。这道检查覆盖 runtime 已发布结束/失败事件、Hub 尚未消费的异步窗口。
7. **旧 settle 事件不能覆盖新 turn。** fold 后若仍有不同 `TurnId` 的 `unsettled_user_message`，`turn_running` 必须保持 `true`；只有没有后继消息时才置为 `false`。

## Implementation Notes

- `Deferred` 仍用于同一 turn 内多个 sub-agent `ToolCall` 撞到同一个 agent 的信箱冲突；只删除了根任务顶替路径。
- bootstrap 现在返回 `Result`，多个不同 `TurnId` 会经 `SessionBuilder::open` 的 `OpenError::Storage` 返回，并保留冲突 id 供诊断。
- Web store 在调用前同步拦截 busy session；请求失败时还原提交前的 `running`，不会用失败回滚覆盖已有状态。
- root loop 的 checkpoint 加载错误复用 `PersistFailed` 作为强制重同步信号；它不是一个已落库的正常结束事件。

## Implementation Roadmap

- [x] [runtime] 将 `ActiveTurns` 收窄为唯一 `ActiveTurn`，在投递前拒绝第二个根 `Task`，并让 bootstrap 对多个不同 `TurnId` 返回错误
      Purpose: 建立不会被调用方绕过的 single-flight 不变量
      Verification: 生成中、等待 sub-agent、等待审批和退出 drain 时均拒绝第二个 Task；结束或 Abort 收场后可再次提交；同一 turn 的恢复证据幂等

- [x] [server] 将 `TurnAlreadyActive` 映射为 JSON-RPC `SESSION_NOT_IDLE`
      Purpose: 给所有客户端稳定、明确的拒绝响应
      Verification: 连续 task request 的第二条返回 `-32006`，且 Hub 不增加 user message、不清 pending approval

- [x] [web] Composer 将 pending approval 计入 busy；修正 rejected task 的 optimistic rollback，使它不覆盖原 turn 状态
      Purpose: 让正常交互符合服务端契约，并正确处理竞态
      Verification: 审批出现后不能发送；模拟 `SESSION_NOT_IDLE` 后旧 turn 仍保持原状态

- [x] [cleanup] 删除仅服务多个根 turn 排队/顶替的代码和测试，保留递归取消及同一 turn 的 sub-agent 仲裁
      Purpose: 将收紧行为兑现成真实的复杂度下降
      Verification: 代码中不再描述 queued root turns；定向取消、审批、重启测试覆盖保留机制

- [x] [validation] 运行 Rust、Web 和 PostgreSQL 全套检查
      Purpose: 验证跨层契约及持久化路径没有回归
      Verification: `cargo clippy`、`cargo test`、`pnpm --filter coda-web lint`、`pnpm --filter coda-web test`、临时 PostgreSQL 数据库上的 `storage_pg` 全绿

- [x] [runtime error] root `AgentLoop` 在 checkpoint 加载失败时发 `PersistFailed` 并释放对应 active turn
      Purpose: 避免 agent loop 已退出而准入槽永久占用
      Verification: 注入 checkpoint load error 后收到一次失败事件，且下一条 Task 不再得到 `TurnAlreadyActive`

- [x] [hub ordering] Hub 增加保守入口检查，并从 fold 后剩余的 unsettled message 推导 `turn_running`
      Purpose: 关闭事件发布与消费之间的窗口，避免旧 settle 把新 turn 标成 idle
      Verification: 延迟处理旧 turn 事件并先登记新 turn 后，旧事件不清除新消息、不将 session 释放

- [x] [review validation] 运行 Rust、Web 和 PostgreSQL 全套检查
      Purpose: 验证两条 review 修复没有改变正常结束、审批与持久化行为
      Verification: `cargo clippy`、`cargo test`、Web lint/test、临时 PostgreSQL 数据库上的 `storage_pg` 全绿
