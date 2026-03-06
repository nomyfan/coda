use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMProvider, Message, SystemMessage, ToolCall,
    ToolDefinition, ToolMessage, ToolOutput,
};
use coda_core::tool::ToolSet;
use tracing::debug;

use crate::tools::{
    GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, ReadTodosTool, ShellTool, WriteFileTool,
    WriteTodosTool,
};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_AGENT_ID: AtomicU32 = AtomicU32::new(0);

fn next_agent_id() -> u32 {
    NEXT_AGENT_ID.fetch_add(1, Ordering::Relaxed)
}

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

pub struct AgentState<S> {
    pub messages: Vec<Message>,
    pub todos: Vec<TodoItem>,
    pub opaque: S,
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

pub struct Agent<S> {
    name: String,
    pub agent_id: u32,
    pub system_prompt: Option<String>,
    pub state: Arc<Mutex<AgentState<S>>>,
    pub tools: ToolSet,
    pub subagents: SubAgents,
}

pub struct RunConfig<P: LLMProvider> {
    pub provider: P,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_completion_tokens: Option<u32>,
    pub thread_id: String,
    pub tool_approval: ToolApprovalMode,
}

impl<S: Send + 'static> Agent<S> {
    pub fn new(name: impl ToString, state: S) -> Self {
        let state = Arc::new(Mutex::new(AgentState {
            messages: vec![],
            todos: vec![],
            opaque: state,
        }));

        Agent {
            agent_id: next_agent_id(),
            name: name.to_string(),
            system_prompt: None,
            state,
            tools: ToolSet::default(),
            subagents: SubAgents::default(),
        }
    }

    pub fn with_default_tools(&mut self, workspace_dir: impl Into<String>) {
        let cwd = workspace_dir.into();
        self.tools.register(ShellTool::new());
        self.tools.register(ReadFileTool::new());
        self.tools.register(WriteFileTool::new());
        self.tools.register(ListDirectoryTool::new());
        self.tools.register(GrepTool::new(cwd.clone()));
        self.tools.register(GlobTool::new(cwd));
        let state = self.state.clone();
        self.tools.register(ReadTodosTool::new(state.clone()));
        self.tools.register(WriteTodosTool::new(state));
    }
}

impl<S> Agent<S> {
    pub fn state(&self) -> Arc<Mutex<AgentState<S>>> {
        self.state.clone()
    }

    pub fn name(&self) -> &str {
        &self.name
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
        let mut messages = self.state.lock().await.messages.clone();
        if let Some(system_prompt) = &self.system_prompt {
            messages.insert(0, Message::System(SystemMessage(system_prompt.clone())));
        }
        return messages;
    }

    /// Append previously saved (non-system) messages and restore todos.
    /// The system message must already have been added before calling this.
    pub async fn restore_history(&self, messages: Vec<Message>, todos: Vec<TodoItem>) {
        let mut state = self.state.lock().await;
        state.messages.extend(messages);
        state.todos = todos;
    }
}

pub enum SubAgent<S> {
    // TODO: 是否有必要用 Mutex
    Stateful(Mutex<Agent<S>>),
    Stateless(Mutex<Agent<()>>),
}

pub struct SubAgentTool<S> {
    pub name: String,
    pub description: String,
    pub agent: SubAgent<S>,
}

pub trait SubAgentObject: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
}

struct SubAgentToolWrapper<S>(SubAgentTool<S>);

impl<S: Send + Sync + 'static> SubAgentObject for SubAgentToolWrapper<S> {
    fn name(&self) -> &str {
        &self.0.name
    }

    fn description(&self) -> &str {
        &self.0.description
    }
}

pub struct SubAgents(Vec<Arc<dyn SubAgentObject>>);

impl Default for SubAgents {
    fn default() -> Self {
        Self(vec![])
    }
}

impl SubAgents {
    pub fn register<S: Send + Sync + 'static>(&mut self, subagent: SubAgentTool<S>) {
        self.0.push(Arc::new(SubAgentToolWrapper(subagent)));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn SubAgentObject>> {
        self.0.iter().find(|agent| agent.name() == name).cloned()
    }

    pub fn descriptors(&self) -> Vec<ToolDefinition> {
        self.0
            .iter()
            .map(|subagent| ToolDefinition {
                name: subagent.name().to_string(),
                description: subagent.description().to_string(),
                parameter_schema: json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "The task to delegate to the sub-agent.",
                        },
                    },
                    "required": ["task"],
                }),
            })
            .collect()
    }
}
