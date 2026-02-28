use std::sync::Arc;
use tokio::sync::Mutex;

use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMProvider, Message, ToolCall, ToolMessage,
    ToolOutput,
};
use coda_core::tool::ToolManager;
use tracing::debug;

use crate::tools::{
    GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, ReadTodosTool, ShellTool, WriteFileTool,
    WriteTodosTool,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub title: String,
    pub done: bool,
}

pub enum ToolApprovalMode {
    Auto,
    Manual,
    RequireWhen(Arc<dyn Fn(&ToolCall) -> bool + Send + Sync>),
}

/// Caller's resolution for a single suspended tool call.
pub enum ToolCallResolution {
    /// The agent should execute this call.
    Execute,
    /// The caller already handled it; use this result directly.
    Resolved(ToolOutput),
    /// The caller rejected execution.
    Rejected { reason: Option<String> },
}

/// Caller's response to all suspended tool calls, replacing `ApprovalDecision`.
pub struct ResumeDecision {
    pub resolutions: Vec<(String, ToolCallResolution)>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentCheckpoint {
    pub thread_id: String,
    pub messages: Vec<Message>,
    /// Tool calls that require human approval before execution.
    pub pending_calls: Vec<ToolCall>,
    /// Tool calls that can be executed automatically without approval.
    pub auto_calls: Vec<ToolCall>,
    pub todos: Vec<TodoItem>,
}

pub struct AgentState {
    pub messages: Vec<Message>,
    pub todos: Vec<TodoItem>,
    // TODO: custom opaque state
}

/// Identifies what was interrupted by an abort.
pub enum AbortedTarget {
    /// LLM generation was interrupted.
    Generation,
    /// Tool execution was interrupted; carries the IDs of unfinished tool calls.
    ToolCalls(Vec<String>),
}

/// Events produced by `Agent::run` and `Agent::resume`.
pub enum AgentEvent {
    LLMStart(ChatCompletionRequest),
    LLMContentChunk(String),
    LLMEnd(AssistantMessage),
    ToolCallStart(ToolCall),
    ToolCallEnd(ToolMessage),
    /// Emitted when tool calls require human approval. The stream terminates after this event.
    /// Call `Agent::resume` with the checkpoint and a `ResumeDecision` to continue.
    Suspended(AgentCheckpoint),
    /// Emitted when the run is aborted by the user. The stream terminates after this event.
    Aborted(AbortedTarget),
}

pub struct Agent {
    pub state: Arc<Mutex<AgentState>>,
    pub tools: ToolManager,
}

pub struct RunConfig<P: LLMProvider> {
    pub provider: P,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_completion_tokens: Option<u32>,
    pub thread_id: String,
    pub tool_approval: ToolApprovalMode,
}

impl Agent {
    pub fn new() -> Self {
        let state = Arc::new(Mutex::new(AgentState {
            messages: vec![],
            todos: vec![],
        }));

        Agent {
            state,
            tools: ToolManager::new(),
        }
    }

    pub fn new_with_default_tools(workspace_dir: impl Into<String>) -> Self {
        let mut agent = Self::new();
        let cwd = workspace_dir.into();
        let state = agent.state.clone();
        agent.tools.register(ShellTool::new());
        agent.tools.register(ReadFileTool::new());
        agent.tools.register(WriteFileTool::new());
        agent.tools.register(ListDirectoryTool::new());
        agent.tools.register(GrepTool::new(cwd.clone()));
        agent.tools.register(GlobTool::new(cwd));
        agent.tools.register(ReadTodosTool::new(state.clone()));
        agent.tools.register(WriteTodosTool::new(state));
        agent
    }

    pub fn state(&self) -> Arc<Mutex<AgentState>> {
        self.state.clone()
    }

    pub async fn add_message(&self, message: Message) {
        debug!("Adding message: {:?}", message);
        self.state.lock().await.messages.push(message);
    }

    pub async fn add_messages(&self, messages: Vec<Message>) {
        debug!("Adding messages: {:?}", messages);
        self.state.lock().await.messages.extend(messages);
    }

    pub async fn messages(&self) -> Vec<Message> {
        self.state.lock().await.messages.clone()
    }

    /// Append previously saved (non-system) messages and restore todos.
    /// The system message must already have been added before calling this.
    pub async fn restore_history(&self, messages: Vec<Message>, todos: Vec<TodoItem>) {
        let mut state = self.state.lock().await;
        state.messages.extend(messages);
        state.todos = todos;
    }
}
