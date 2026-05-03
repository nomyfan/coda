## 问题

当前 agent runtime 实现的是同步 HITL：审批决策必须在同一进程内、即时提供。要做 client-server demo，需要异步 HITL —— 审批请求能够被持久化、被另一个进程发现、并在任意时间点被处理。

## 范围

**包含：** Runtime API 层的改动，使审批状态对外可见、加载 session 时无需持有内存中的 checkpoint 即可恢复、跨崩溃安全持久化。
**不包含：** 网络协议、线格式、事件的序列化传输、client SDK、server 脚手架 —— 这些属于 client-server demo 层。

## 假设

- Agent 在 `PendingApproval` 时仍然必须**暂停** —— 没有工具结果就无法继续生成。"异步"指的是 *调用方* 解耦，而不是 agent 在等待审批期间能继续工作。

- **每次 "turn" 是一次 Session 生命周期。** 发送任务 → 消费事件流 → 遇到审批挂起或正常完成 → session 退出。审批决策通过新 session 的 `resume_decisions` 注入，而不是在运行中的 session 上调用 `resume()`。这个模型天然适合无状态协议（HTTP、gRPC），同时也适用于 CLI 循环重建 session。

- 单一 `SessionStorage` 实现（基于文件）足以支撑 demo；不考虑可插拔后端。

- 现有的 envelope/thread 模型是正确的，保持不变。

- 不存在并发的审批响应者 —— 两个 client 不会同时审批同一个 thread。不加分布式互斥。

## 当前状态回顾

```
调用方                   Session/AgentRuntime        存储
  |                           |                       |
  |-- send(task) ------------>|                       |
  |                           |--[LLM 生成]           |
  |<-- Suspended(checkpoint)--|                       |
  |                           |--save_checkpoint----->|
  |-- resume(checkpoint, decision) ->|                |
  |                           |--[执行工具]           |
  |<-- events ...             |                       |
```

关键限制：

- `Session::resume()` 要求传入 `&AgentCheckpoint` —— 调用方必须持有从 `Suspended` 事件收到的 checkpoint 对象。
- Resume 发生在同一个 session 生命周期内 —— agent task 在后台持续运行、等待 Resume envelope。session 必须保持活跃。
- 运行中的 `Session` 没有方法列出待审批项。
- `AgentRuntimeSnapshot`（信封队列 + 活跃线程）仅在 `wait_for_exit` 时持久化 —— `runtime.rs:349` 的 TODO 指出这会在崩溃时丢失状态。
- `suspended_at` 在每次 checkpoint 保存时被覆盖 —— `driver.rs:265` 的 TODO 指出它不能反映真正的挂起时间。

## 核心设计决策：Session 生命周期模型

**旧模型**：Session 持续运行，agent 在 PendingApproval 状态**等待** Resume envelope（常驻内存）。

```
open ── send ── [run] ── PendingApproval ── [waiting...] ── resume ── [run] ── done ── shutdown
                            │
                            └── agent task 阻塞，session 不退出
```

**新模型**：Agent 在 PendingApproval 状态**退出**。Session 关闭，下次调用者带着审批结果重新 open。

```
Turn 1:  open ── send ── [run] ── PendingApproval ── exit agent ── shutdown
                                                       │
                          caller 收集审批决策 ◄────────┘

Turn 2:  open (with resume_decisions) ── [run tools] ── [run] ── done ── shutdown
```

### 为什么这适合两种 HITL 模式

**Sync（CLI）**：CLI 在循环里反复 open/close session —— 开销是本地文件读写，微秒级，人类无感知。

```
loop {
    session = Session::builder().session_id(id).resume_decisions(prev_decisions).open()
    session.send(input)
    for event in session.recv():
        case Suspended(pending): decisions = prompt_user(pending); break
        case LLMEnd: break
    session.shutdown(graceful, timeout=2s)
}
```

**Async（client-server）**：Server 每次 HTTP 请求都是一个独立的 session。

```
POST /chat { session_id, task }          → 启动 session，跑到底，返回事件流 + pending approvals
POST /resume { session_id, decisions }   → 用 decisions 启动 session，继续跑
```

## API 设计

### `AgentCheckpoint` 退为内部类型

`AgentCheckpoint` 不再出现在任何公共 API 中。它是 runtime 和 storage 之间的内部持久化格式。调用方只需知道 `PendingApproval`。

### 新类型 `PendingApproval`

```rust
/// 一个等待审批的 agent 线程的快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub thread_id: String,
    pub agent_name: String,
    pub calls: Vec<ToolCall>,
    pub suspended_at: jiff::Timestamp,
}
```

### 修改 `AgentEvent::Suspended`

```rust
pub enum AgentEvent {
    // ... 其他变体不变 ...
    /// 工具调用需要人工审批。此事件后，该 agent 线程退出；
    /// 调用者应 shutdown session，收集决策，通过新 session 的
    /// resume_decisions 继续执行。
    Suspended(PendingApproval),
    // ...
}
```

### `Session` API —— 精简

```rust
impl Session {
    /// 创建 builder
    pub fn builder<P: LLMProvider + Clone + 'static>() -> SessionBuilder<P>;

    /// Session ID
    pub fn session_id(&self) -> &str;

    /// 根 agent 名称
    pub fn root_name(&self) -> &str;

    /// 发送任务
    pub async fn send(&self, task: impl Into<String>) -> Result<(), SendCommandError>;

    /// 接收下一个事件
    pub async fn recv(&self) -> Option<SessionEvent>;

    /// 恢复前对话历史（重新 open 时可用）
    pub fn resumed_checkpoint(&self) -> Option<&AgentCheckpoint>;

    /// 是否有恢复中的 agent（用于跳过用户输入）
    pub fn has_resuming_agents(&self) -> bool;

    /// 强制中止所有 agent 当前工作
    pub async fn abort(&self);

    /// 关闭 session，按指定策略等待 agent 退出
    pub async fn shutdown(&self, mode: Shutdown) -> bool;
}
```

**移除的方法**：
- ~~`Session::resume(&AgentCheckpoint, decision)`~~ —— 所有恢复通过 `SessionBuilder::resume_decisions()` + `open()` 完成
- ~~`Session::pending_approvals()`~~ —— 不需要了。待审批信息通过两个渠道获取：(1) 运行中的 `Suspended` 事件；(2) 重新 open 时的 `OpenError::PendingApprovalsRequired`

### `SessionBuilder` —— 保持现有 API

```rust
impl SessionBuilder<P> {
    // ... 既有方法保持不变 ...
    pub fn resume_decisions(mut self, decisions: HashMap<String, ResumeDecision>) -> Self;
    pub async fn open(self) -> Result<Session, OpenError>;
}
```

`OpenError::PendingApprovalsRequired(Vec<AgentCheckpoint>)` 仍然存在，但这里的 checkpoint 需要转换一下 —— 或者直接改为返回 `Vec<PendingApproval>`：

```rust
pub enum OpenError {
    // ... 其他变体不变 ...
    /// 存在待审批的线程，但 resume_decisions 未覆盖。
    /// 调用者应检查返回的 PendingApproval 列表，收集决策后重新 open。
    PendingApprovalsRequired(Vec<PendingApproval>),
}
```

### 调用方 API 使用

**Sync 场景（CLI）核心循环：**

```rust
let mut session_id = Uuid::new_v4().to_string();
let mut decisions = HashMap::new();

loop {
    // 1. 打开 session（可能带上次的审批决策）
    let session = match Session::builder()
        .storage(storage.clone())
        .root(spec.clone())
        .build_context(ctx.clone())
        .run_config(config.clone())
        .session_id(&session_id)
        .resume_decisions(decisions)
        .open()
        .await
    {
        Ok(s) => s,
        Err(OpenError::PendingApprovalsRequired(pending)) => {
            // 2. 有未处理的审批 → 收集决策后重试
            decisions = collect_decisions_interactive(&pending);
            continue;
        }
        Err(e) => return Err(e.into()),
    };

    // 3. 发送用户输入
    let input = readline();
    session.send(input).await?;

    // 4. 消费事件流
    while let Some(event) = session.recv().await {
        match event.kind {
            AgentEvent::Suspended(pending) => {
                // 记下待审批项，退出事件循环
                decisions = prompt_user(&pending);
                break;
            }
            AgentEvent::LLMEnd(msg) if event.origin.is_root() && msg.tool_calls.is_empty() => {
                // 根 agent 完成回复
                break;
            }
            // ... 渲染其他事件
            _ => {}
        }
    }

    // 5. 安全关闭
    session.shutdown(Shutdown::graceful(Duration::from_secs(2))).await;

    // 如果本次没有挂起审批（正常完成），重置决策
    if /* 没有 Suspended */ {
        decisions.clear();
    }
}
```

## 内部运行模型

### Agent 循环行为

当前 agent 循环在 `PendingApproval` 时：
1. 保存 checkpoint
2. 发出 `Suspended` 事件
3. `break` 内层循环
4. 外层循环回到等待下一个 envelope（阻塞在 `envelope_rx.recv()`）

改为：
1. 保存 checkpoint
2. 发出 `Suspended` 事件
3. Agent 循环退出（等同于收到 Exit 信号）
4. `save_agent_snapshot` 保存信封队列和活跃线程状态

### Agent 循环恢复行为

新 session open 时，如果 resume_decisions 覆盖了 PendingApproval checkpoint，agent 在 `handle_envelope` 中：
1. 处理 Resume envelope 中的决议 → 将 pending_approval_calls 转换为 ToolExecution 的 pending_calls
2. 转回 `AgentLoopState::Next(ResumePoint::ToolExecution(...))`
3. 主循环进入 `handle_tool_execution`，执行工具
4. 工具执行完毕后进入 `Generation`，视情况继续 LLM 生成

**现有代码已经支持这个恢复路径**（driver.rs 的 `handle_envelope` 中 `PendingApproval` 分支处理了 `EnvelopeBody::Resume`）。需要改的只是退出逻辑。

### Session 退出流程

1. Caller 调用 `session.shutdown(Shutdown::Graceful { timeout, on_timeout })`
2. Runtime 广播 `Exit` 给所有 agent task
3. 每个 agent task 退出循环，drain 残留 envelope，保存 snapshot
4. `wait_for_exit` 等所有 task join
5. 保存 session snapshot
6. 如果超时且 `on_timeout == Abort`，依次 abort + exit

## 组件改动总结

### 1. 新增 `PendingApproval` 类型（agent.rs）

轻量级审批摘要，出现在 `AgentEvent::Suspended` 和 `OpenError::PendingApprovalsRequired` 中。

### 2. `AgentEvent::Suspended` 改为携带 `PendingApproval`（agent.rs）

从携带 `AgentCheckpoint` 改为 `PendingApproval`。`AgentCheckpoint` 退为内部类型。

### 3. Agent 循环改为退出而非等待（driver.rs）

`PendingApproval` 状态下 agent 循环退出，不再阻塞等待 Resume envelope。Resume envelope 逻辑保留（用于新 session open 时 bootstrap 注入的 resume_decisions）。

### 4. `AgentRuntime::save_agent_snapshot` 持久化快照（runtime.rs）

在 agent 退出时立即写入 snapshot 到 storage，不仅仅缓存在内存。修复 `runtime.rs:349` 的 TODO。

### 5. `suspended_at` 时间戳修复（driver.rs）

仅在进入 `PendingApproval` 时设置一次，不被后续 `save_checkpoint` 覆盖。修复 `driver.rs:265` 的 TODO。

### 6. `OpenError::PendingApprovalsRequired` 改为携带 `Vec<PendingApproval>`（session.rs）

返回轻量的 `PendingApproval` 列表而非 `AgentCheckpoint` 列表。

### 7. `Session` —— 移除 `resume()` 方法（session.rs）

```diff
- pub async fn resume(&self, checkpoint: &AgentCheckpoint, decision: ResumeDecision) -> Result<(), SendCommandError>;
+ // 所有恢复通过 SessionBuilder::resume_decisions() + open() 完成
```

### 8. `RunConfig` —— 新增 `approval_timeout`（agent.rs）

```rust
pub struct RunConfig<P: LLMProvider> {
    // ... 既有字段 ...
    /// 如果设置，agent 进入 PendingApproval 超过此时间后，
    /// 外部未提供决议的调用将被自动拒绝。
    pub approval_timeout: Option<Duration>,
}
```

超时后自动拒绝同样产生 `Suspended` 事件（pending calls 已处理完毕），但 `calls` 为空。调用方按正常流程 shutdown 后重新 open 即可继续。

## 数据模型

### 公共 API 层

```rust
pub struct PendingApproval {
    pub thread_id: String,        // resume_decisions 的 key
    pub agent_name: String,       // 展示用：哪个 agent 在请求审批
    pub calls: Vec<ToolCall>,     // 待审批的工具调用
    pub suspended_at: Timestamp,  // 挂起时间
}
```

### 内部持久化层

`AgentCheckpoint` 保持不变，作为 `SessionStorage` 的内部格式。`PendingApproval` 在需要时从 checkpoint 投影构造。

## 调用流程

### Sync 场景（CLI）

```
Turn 1:
  open(new session) → send("run a shell command") → recv events
  → Suspended { thread_id, calls: [shell] }
  → shutdown(graceful)
  → 提示用户批准/拒绝

Turn 2:
  open(same session_id, resume_decisions={thread_id: [approved]})
  → recv events (工具执行 + 后续 LLM 输出)
  → LLMEnd
  → shutdown(graceful)
```

### Async 场景（client-server）

```
Request 1: POST /chat { session_id: "abc", task: "run a shell command" }
  → Server open session "abc", send task, consume events
  → 遇到 Suspended，shutdown
  → Response: { status: "pending_approval", approvals: [{ thread_id, calls: [shell] }] }

Request 2: POST /resume { session_id: "abc", decisions: { thread_id: [approved] } }
  → Server open session "abc" with resume_decisions, consume events
  → 正常完成
  → Response: { status: "done", events: [...] }
```

## 风险 / 待确认问题

1. **根 agent 挂起时，正在运行的子 agent 调用会怎样？** 子 agent 继续运行，回复缓冲在 `drained_envelopes`。快照持久化修复确保它们跨越 session 重建时存活。这个边界情况值得专门测试。

2. **`suspended_at` 与超时的交互。** Agent 循环退出后 session 关闭了，所以超时不是靠"agent task 内部 sleep"来驱动的 —— agent 已经退出了。超时应由 storage 层的时间戳来判断。具体来说：新 session open 时检查 pending checkpoint 的 `suspended_at`，如果超过 `approval_timeout` 且没有对应的 `resume_decisions`，自动生成拒绝决议。这个逻辑放在 `collect_pending_approvals` 或 `open()` 中。

3. **`OpenError::PendingApprovalsRequired` 中返回 `Vec<PendingApproval>`。** 这个错误被触发后 caller 会重新 open。每次 open 都会加载所有 agent checkpoint。对少量 agent 的 demo 没问题，生产环境建议加缓存。

## 实现路线图

- [ ] [bugfix] 修复 `driver.rs` 中的 `suspended_at` —— 仅在进入 `PendingApproval` 时设置一次
  - 目的：使超时判断有意义
  - 验证：单元测试 —— 确认 `suspended_at` 在后续 checkpoint 保存中不被覆盖

- [ ] [bugfix] 在 `save_agent_snapshot()` 中持久化 `AgentRuntimeSnapshot`（runtime.rs ~349 行）
  - 目的：验证急切快照持久化正确且不引入竞争
  - 验证：agent 挂起退出后，用相同 session_id 执行新 `Session::open`，能找到正确的 `active_threads` 和 `drained_envelopes`

- [ ] [core] 新增 `PendingApproval` 类型，修改 `AgentEvent::Suspended` 携带它，修改 `OpenError::PendingApprovalsRequired` 携带 `Vec<PendingApproval>`
  - 目的：弱化 caller 对 checkpoint 的感知
  - 验证：编译通过，现有测试适配后通过

- [ ] [core] 修改 agent 循环：`PendingApproval` 状态下 agent 直接退出，不等待 Resume envelope
  - 目的：实现 "每次 turn 一个 session 生命周期"
  - 验证：集成测试 —— session.send() 触发审批 → recv 收到 Suspended → shutdown → 新 session open 带 resume_decisions → agent 继续执行完成

- [ ] [core] 将超时检查逻辑放在 `open()` / `collect_pending_approvals` 中（而非 agent 循环内）
  - 目的：配合 "session 关闭" 模型，超时基于持久化的 `suspended_at` 判断
  - 验证：打开 session 且 pending checkpoint 的 `suspended_at` 已超时 → 自动拒绝

- [ ] [feature] 在 `RunConfig` 中新增 `approval_timeout: Option<Duration>`
  - 目的：防止无限等待审批
  - 验证：单元测试用短超时，确认超时后 pending 被自动拒绝

- [ ] [integration] 更新 `coda_cli` 适配新 API（循环 open/close session）
  - 目的：在新模型上自验 sync 流程
  - 验证：`cargo run -p coda_cli` —— 触发 shell 命令，批准它，观察正常完成
