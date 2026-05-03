## Problem

提供一个 HTTP client-server demo，展示异步 HITL：审批请求跨进程、跨时间传递，不依赖共享内存。

## Scope

**In:** HTTP API 设计、wire format（JSON）、coda_server crate（一个 lib + 两个 bin：server 和 client）、JsonFileStorage 拷贝到 coda_server。

**Out:** 认证、TLS、流式传输（SSE/WebSocket）、多租户、生产级错误处理、分布式存储、client SDK 生成（OpenAPI 等）。

## Assumptions

- 单用户、单 server 实例，不存在并发审批冲突。
- 每次 turn 是一个 HTTP request-response（匹配 per-turn session 生命周期）。
- Session 数据通过文件系统持久化；server 和 client 共享同一个 checkpoint 目录（demo 阶段可接受）。
- Client 是 CLI 工具，行为类似现有 coda_cli，但通过 HTTP 调 server 而非内嵌 Session。
- LLM provider 配置在 server 端，client 不感知。

## Components

- **coda_server** — 一个 crate，两个 bin：
  - `server` bin — axum HTTP server，接受 ChatRequest，创建 Session，运行一个 turn，收集事件，返回 ChatResponse。无状态（session 状态在 storage 中）。
  - `client` bin — reqwest CLI 客户端，管理 session_id 生命周期，循环发送请求、渲染事件、收集审批决策。
  - **lib** — crate 内的共享层：wire 类型定义（`WireEvent`、`ChatRequest`、`ChatResponse`）、`From<SessionEvent>` 转换。server 和 client 两个 bin 都依赖 lib。
- **JsonFileStorage** — 从 `coda_cli/src/storage.rs` 拷贝到 `coda_server/src/storage.rs`。coda_agent 无需修改，coda_cli 保留原副本。

## Interfaces

### HTTP API

```
POST /chat
```

**Request** (JSON):

```rust
struct ChatRequest {
    session_id: String,                              // 新 session 用 UUID，续接用已有的
    task: Option<String>,                            // 新用户输入（纯 resume 时为 None）
    resume_decisions: HashMap<String, ResumeDecision>, // key = thread_id
}
```

**Response** (JSON):

```rust
struct ChatResponse {
    status: ChatStatus,
    events: Vec<WireEvent>,                             // 本 turn 的所有事件（已转换为 wire 格式）
    pending_approvals: Vec<PendingApproval>,         // status == PendingApproval 时非空
}

enum ChatStatus {
    Done,                   // 正常结束，根 agent 完成回复
    PendingApproval,        // 需要人工审批，client 应收集决策后再次请求
    Error(String),          // 不可恢复错误
}
```

**Turn 流程**：

```
Client                           Server
  |                                 |
  |-- POST /chat { session_id, task } -->|
  |                                 |-- Session::open()
  |                                 |-- session.send(task)
  |                                 |-- loop recv() → collect events
  |                                 |-- session.shutdown()
  |<-- { status: "pending_approval", events, pending_approvals } --|
  |                                 |
  |   [user makes decisions]       |
  |                                 |
  |-- POST /chat { session_id, resume_decisions } -->|
  |                                 |-- Session::open() with resume_decisions
  |                                 |-- has_resuming_agents → enter event loop
  |                                 |-- session.shutdown()
  |<-- { status: "done", events } --|
```

### Server 内部接口

Server handler 的核心逻辑（伪代码）：

```rust
async fn chat_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, AppError> {
    // 1. 打开 session
    let mut builder = Session::builder()
        .storage(state.storage.clone())
        .root(state.agent_spec.clone())
        .build_context(state.build_context.clone())
        .run_config(state.run_config.clone())
        .session_id(&req.session_id)
        .resume_decisions(req.resume_decisions);

    let session = match builder.open().await {
        Ok(s) => s,
        Err(OpenError::PendingApprovalsRequired(pending)) => {
            // caller 的 resume_decisions 不完整 → 直接返回待审批列表
            return Ok(Json(ChatResponse {
                status: ChatStatus::PendingApproval,
                events: vec![],
                pending_approvals: pending,
            }));
        }
        Err(e) => return Err(AppError::Open(e)),
    };

    // 2. 发送任务（如果有）
    if let Some(task) = req.task {
        session.send(task).await?;
    }

    // 3. 如果本轮有恢复中的 agent，直接进入事件循环；否则等待 send 触发工作
    let mut events: Vec<WireEvent> = Vec::new();
    let mut pending_approvals: Vec<PendingApproval> = Vec::new();
    let mut status = ChatStatus::Done;

    while let Some(event) = session.recv().await {
        match &event.kind {
            AgentEvent::Suspended(pending) => {
                pending_approvals.push(pending.clone());
                status = ChatStatus::PendingApproval;
                events.push(event.into());
                break;
            }
            AgentEvent::LLMEnd(msg) if event.origin.is_root() && msg.tool_calls.is_empty() => {
                events.push(event.into());
                break;
            }
            _ => events.push(event.into()),
        }
    }

    session.shutdown(Shutdown::graceful(Duration::from_secs(5))).await;

    Ok(Json(ChatResponse { status, events, pending_approvals }))
}
```

## Data Model

### 设计原则：传输层与 agent SDK 解耦

`AgentEvent` / `SessionEvent` 属于 agent SDK domain —— 它们携带运行时语义，内部结构可能随重构变化。Wire format 属于传输层 domain —— 它是 client 和 server 之间的合约，需要独立演进。

**Agent 内部类型**（不加 Serialize）：
- `AgentEvent`、`SessionEvent`、`EventOrigin` — 保持 `#[derive(Debug, Clone)]`

**Wire 类型**（`coda_server` crate lib，全部 Serialize/Deserialize）：
- `WireEvent` — 扁平化的 JSON 事件，替代嵌套的 `SessionEvent { origin, thread_id, kind: AgentEvent }`
- `ChatRequest`、`ChatResponse`、`ChatStatus` — HTTP API 的请求/响应

**转换**：`impl From<SessionEvent> for WireEvent`，在 server handler 中 events 收集完毕后批量转换。

### Wire 类型定义

```rust
// coda_server crate — src/lib.rs (或 src/wire.rs)

/// 一个扁平的事件，包含 agent 身份 + 事件数据。
/// JSON 通过 `#[serde(tag = "type")]` 区分 variant。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WireEvent {
    #[serde(rename = "llm_start")]
    LlmStart {
        agent_name: String,
        thread_id: String,
        model: String,
    },
    #[serde(rename = "llm_chunk")]
    LlmContentChunk {
        agent_name: String,
        thread_id: String,
        content: String,
    },
    #[serde(rename = "llm_end")]
    LlmEnd {
        agent_name: String,
        thread_id: String,
        message: AssistantMessage,
    },
    #[serde(rename = "tool_start")]
    ToolCallStart {
        agent_name: String,
        thread_id: String,
        call: ToolCall,
    },
    #[serde(rename = "tool_end")]
    ToolCallEnd {
        agent_name: String,
        thread_id: String,
        message: ToolMessage,
    },
    #[serde(rename = "suspended")]
    Suspended {
        agent_name: String,
        thread_id: String,
        approval: PendingApproval,
    },
    #[serde(rename = "aborted")]
    Aborted {
        agent_name: String,
        thread_id: String,
        target: AbortedTargetWire,
    },
    #[serde(rename = "error")]
    Error {
        agent_name: String,
        thread_id: String,
        message: String,
    },
}

/// Wire 版本的 AbortedTarget —— 独立于 agent 内部类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reason")]
pub enum AbortedTargetWire {
    Generation,
    ToolCalls { call_ids: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub session_id: String,
    /// None when this turn is a pure resume (no new user input).
    pub task: Option<String>,
    /// Key = thread_id from PendingApproval.
    pub resume_decisions: HashMap<String, ResumeDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub status: ChatStatus,
    pub events: Vec<WireEvent>,
    /// Non-empty only when status == PendingApproval.
    pub pending_approvals: Vec<PendingApproval>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatStatus {
    Done,
    PendingApproval,
    Error(String),
}
```

### 转换：SessionEvent → WireEvent

```rust
impl From<SessionEvent> for WireEvent {
    fn from(event: SessionEvent) -> Self {
        let agent_name = match &event.origin {
            EventOrigin::Root => "root".to_string(),  // 或使用 session.root_name()
            EventOrigin::Sub { name } => name.clone(),
        };
        let thread_id = event.thread_id.0;

        match event.kind {
            AgentEvent::LLMStart(req) => WireEvent::LlmStart {
                agent_name,
                thread_id,
                model: req.model,
            },
            AgentEvent::LLMContentChunk(content) => WireEvent::LlmContentChunk {
                agent_name, thread_id, content,
            },
            AgentEvent::LLMEnd(msg) => WireEvent::LlmEnd {
                agent_name, thread_id, message: msg,
            },
            AgentEvent::ToolCallStart(call) => WireEvent::ToolCallStart {
                agent_name, thread_id, call,
            },
            AgentEvent::ToolCallEnd(msg) => WireEvent::ToolCallEnd {
                agent_name, thread_id, message: msg,
            },
            AgentEvent::Suspended(approval) => WireEvent::Suspended {
                agent_name, thread_id, approval,
            },
            AgentEvent::Aborted(target) => WireEvent::Aborted {
                agent_name,
                thread_id,
                target: target.into(),
            },
            AgentEvent::Error(msg) => WireEvent::Error {
                agent_name, thread_id, message: msg,
            },
        }
    }
}

impl From<AbortedTarget> for AbortedTargetWire {
    fn from(t: AbortedTarget) -> Self {
        match t {
            AbortedTarget::Generation => AbortedTargetWire::Generation,
            AbortedTarget::ToolCalls(ids) => AbortedTargetWire::ToolCalls { call_ids: ids },
        }
    }
}
```

### 复用现有可序列化类型

以下类型已有 `Serialize/Deserialize`，wire 类型直接引用，无需修改：

| 类型 | 来源 | 用途 |
|---|---|---|
| `PendingApproval` | `coda_agent::agent` | Suspended 事件的审批信息 |
| `ResumeDecision` | `coda_agent::agent` | 请求中的审批决策 |
| `ToolCallResolution` | `coda_agent::agent` | ResumeDecision 的组成部分 |
| `ToolCall` | `coda_core::llm` | 工具调用描述 |
| `ToolMessage` | `coda_core::llm` | 工具执行结果 |
| `AssistantMessage` | `coda_core::llm` | LLM 回复 |
| `ChatCompletionRequest` | `coda_core::llm` | ~~LLM 请求参数~~ 不序列化，LlmStart 只取 `model` 字段 |

### 存储

`JsonFileStorage` 从 `app/coda_cli/src/storage.rs` 拷贝到 `app/coda_server/src/storage.rs`。Server 直接依赖 `coda_agent` 使用 `SessionStorage` trait，coda_agent 无需修改，coda_cli 保留原副本不变。

## Load-Bearing Decisions

1. **独立 wire 类型 vs AgentEvent 直接序列化。** 选择独立 wire 类型，放在 `coda_server` crate 的 lib 中，仅 server 和 client 两个 bin 共享。`coda_agent` 完全不感知传输层。权衡：多一层 `From<SessionEvent>` 转换代码。收益：传输层合约与 agent SDK 解耦在 crate 边界上；JSON 结构可以设计得更扁平（`#[serde(tag = "type")]` 替代嵌套 enum）；内部重构不会意外改变 API 响应格式。

2. **单一 `/chat` endpoint vs `/chat` + `/resume`。** 选择单一 endpoint。SessionBuilder 已经统一了两条路径（`send(task)` + `resume_decisions`），拆成两个 endpoint 反而增加 client 的状态判断逻辑。

3. **Request-response vs 流式。** 选择批量 request-response。Per-turn session 模型天然匹配：session 打开、运行到挂起或完成、关闭。流式（SSE）在审批挂起场景下收益有限 —— LLM 输出被审批截断时，下一个 chunk 是什么并不重要。后续如需流式，可以在 events 数组上加一个顶层 `stream: true` 开关，切换到 SSE。

4. **JsonFileStorage 拷贝而非提升。** 选择直接拷贝到 coda_server。coda_agent 零修改，coda_cli 保留原副本。两个副本独立演进，无耦合风险。如果未来需要统一，再提升到 coda_agent。

## Risks / Open Questions

1. **Server 和 client 共享文件系统。** Demo 假设同一台机器。如果 server 远程部署，需要把 JsonFileStorage 换成 S3 之类的远程存储。SessionStorage trait 已支持这种替换，不是问题。

2. **Server 端错误恢复。** 如果 server 在处理请求时崩溃，checkpoint 已持久化，下次 open 会自动恢复。但正在执行的 tool call 可能产生副作用（如已写入文件）。这和同步模型的风险相同，不是 async 引入的新问题。

3. **resume_decisions 的 key 是 thread_id。** Client 需要记录 `PendingApproval.thread_id` 并在下次请求中带上。这个 thread_id 对调用方是不透明的字符串，没有语义负担。

## Implementation Roadmap

- [x] [wire types] 在 `app/coda_server/src/lib.rs` 中定义 `WireEvent`、`AbortedTargetWire`、`ChatRequest`、`ChatResponse`、`ChatStatus`，实现 `From<SessionEvent> for WireEvent`
  - Purpose: 建立传输层数据模型，server 和 client 两个 bin 共享
  - Verification: 编译通过，`serde_json::to_string` + round-trip 正确

- [x] [server + client] 实现 `app/coda_server`：拷贝 JsonFileStorage、server bin（axum `/chat` endpoint）、client bin（reqwest CLI）
  - Purpose: 第一个可用的 async HITL client-server，coda_agent 零修改
  - Verification: `cargo run -p coda_server --bin server` 启动，`cargo run -p coda_server --bin client` 发起对话

- [x] [integration] 端到端手动测试：server 后台运行，client 交互式对话，触发审批
  - Purpose: 确认整个 async HITL 链路可用
  - Verification: 完整对话 + 审批流程走通
