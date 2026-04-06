use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMProvider, Message, SystemMessage, ToolCall,
    ToolCallOutcome, ToolDefinition, ToolMessage, ToolOutput,
};
use coda_core::tool::ToolSet;
use tracing::debug;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub title: String,
    pub done: bool,
}

#[derive(Clone, Default)]
pub enum ToolApprovalMode {
    #[default]
    Auto,
    Manual,
    RequireWhen(Arc<dyn Fn(&ToolCall) -> bool + Send + Sync>),
}

/// Caller's resolution for a single suspended tool call.
#[derive(Debug, Clone)]
pub enum ToolCallResolution {
    /// The agent should execute this call.
    Execute,
    /// The caller already handled it; use this result directly.
    Resolved(ToolOutput),
    /// The caller rejected execution.
    Rejected { reason: Option<String> },
}

/// Caller's response to all suspended tool calls, replacing `ApprovalDecision`.
#[derive(Debug, Clone)]
pub struct ResumeDecision {
    pub resolutions: Vec<(String, ToolCallResolution)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentCheckpoint {
    pub thread_id: String,
    pub root_thread_id: String,
    pub agent_name: String,
    #[serde(default)]
    pub reply_target: Option<ReplyTarget>,
    pub messages: Vec<Message>,
    pub todos: Vec<TodoItem>,
    pub resume_point: ResumePoint,
    pub suspended_at: jiff::Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyTarget {
    pub envelope_id: String,
    pub sender_name: String,
    pub sender_thread_id: String,
    pub call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingReply {
    pub call_id: String,
    /// Also the name of the peer agent
    pub tool_name: String,
    pub outcome: ToolCallOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionState {
    /// Replies waiting from stateful sub-agents.
    pub pending_replies: Vec<PendingReply>,
    pub tool_calls: VecDeque<PendingToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingToolCall {
    pub tool_call: ToolCall,
    pub outcome: ToolCallOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum ResumePoint {
    #[default]
    Generation,
    ToolExecution(ToolExecutionState),
    PendingApproval {
        /// Tool calls waiting for approval.
        pending_approval_calls: VecDeque<ToolCall>,
        /// Tool calls to execute.
        pending_calls: VecDeque<PendingToolCall>,
    },
}

pub struct AgentState {
    pub messages: Vec<Message>,
    pub todos: Vec<TodoItem>,
}

/// Identifies what was interrupted by an abort.
#[derive(Debug, Clone)]
pub enum AbortedTarget {
    /// LLM generation was interrupted.
    Generation,
    /// Tool execution was interrupted; carries the IDs of unfinished tool calls.
    ToolCalls(Vec<String>),
}

#[derive(Eq, Hash, PartialEq, Clone, Debug)]
pub struct ThreadId(pub(crate) String);

impl Default for ThreadId {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadId {
    pub fn new() -> Self {
        ThreadId(Uuid::new_v4().to_string())
    }

    pub fn from_uuid5(namespace: &ThreadId, name: &str) -> Self {
        let ns = Uuid::parse_str(&namespace.0).unwrap_or(Uuid::nil());
        ThreadId(Uuid::new_v5(&ns, name.as_bytes()).to_string())
    }
}

impl AsRef<str> for ThreadId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for ThreadId {
    fn from(s: String) -> Self {
        ThreadId(s)
    }
}

/// The sender of an envelope.
#[derive(Debug, Clone)]
pub enum Sender {
    /// Message from the user.
    User,
    /// Message from another agent.
    Agent { name: String, thread_id: ThreadId },
}

#[derive(Debug, Clone)]
pub struct Receiver {
    pub name: String,
    pub thread_id: ThreadId,
}

#[derive(Debug, Clone)]
pub enum EnvelopeBody {
    Task(String),
    /// Call agent as a tool
    ToolCall {
        call_id: String,
        task: String,
    },
    /// Reply from a agent, containing the tool output.
    Reply {
        call_id: String,
        output: ToolOutput,
    },
    Resume(ResumeDecision),
}

/// An envelope is a message delivered to an agent, containing the message body and metadata.
#[derive(Debug, Clone)]
pub struct Envelope {
    /// A unique identifier for this message, used for tracking and replying.
    pub id: String,
    /// Sender of the message.
    pub from: Sender,
    /// Receiver of the message.
    pub to: Receiver,
    /// If this message is a reply to another message, this field contains the ID of the original message. Otherwise, it is None.
    pub reply_to: Option<String>,
    /// The content of the message.
    pub body: EnvelopeBody,
}

impl Envelope {
    pub fn with_id(f: impl FnOnce(String) -> Self) -> Self {
        f(Uuid::new_v4().to_string())
    }
}

/// Events produced by `Agent::run` and `Agent::resume`.
#[derive(Debug, Clone)]
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
    Error(String), // TODO: make this more structured
}

pub struct Agent {
    pub name: String,
    pub mode: SubAgentMode,
    pub system_prompt: String,
    pub state: Arc<Mutex<AgentState>>,
    pub tools: ToolSet,
    pub subagents: SubAgents,
}

pub struct RunConfig<P: LLMProvider> {
    pub provider: P,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_completion_tokens: Option<u32>,
    pub tool_approval: ToolApprovalMode,
}

impl<P: LLMProvider + Clone> Clone for RunConfig<P> {
    fn clone(&self) -> Self {
        RunConfig {
            provider: self.provider.clone(),
            model: self.model.clone(),
            temperature: self.temperature,
            max_completion_tokens: self.max_completion_tokens,
            tool_approval: self.tool_approval.clone(),
        }
    }
}

impl Agent {
    pub fn state(&self) -> Arc<Mutex<AgentState>> {
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

    pub async fn todos(&self) -> Vec<TodoItem> {
        self.state.lock().await.todos.clone()
    }

    pub async fn messages(&self) -> Vec<Message> {
        let mut messages = self.state.lock().await.messages.clone();
        messages.insert(
            0,
            Message::System(SystemMessage(self.system_prompt.clone())),
        );
        messages
    }

    /// Returns conversation history without the system prompt (suitable for checkpointing).
    pub async fn history(&self) -> Vec<Message> {
        self.state.lock().await.messages.clone()
    }

    pub async fn restore_history(&self, messages: Vec<Message>, todos: Vec<TodoItem>) {
        let mut state = self.state.lock().await;
        // Filter out any SystemMessage that may have been saved in old checkpoints.
        state.messages = messages
            .into_iter()
            .filter(|m| !matches!(m, Message::System(_)))
            .collect();
        state.todos = todos;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubAgentMode {
    Stateless,
    Stateful,
}

pub struct SubAgentTool {
    pub name: String,
    pub description: String,
    pub mode: SubAgentMode,
}

#[derive(Clone, Default)]
pub struct SubAgents(Vec<Arc<SubAgentTool>>);

impl SubAgents {
    pub fn register(&mut self, subagent: SubAgentTool) {
        self.0.push(Arc::new(subagent));
    }

    pub fn get(&self, name: &str) -> Option<Arc<SubAgentTool>> {
        self.0.iter().find(|agent| agent.name == name).cloned()
    }

    pub fn descriptors(&self) -> Vec<ToolDefinition> {
        self.0
            .iter()
            .map(|subagent| ToolDefinition {
                name: subagent.name.to_string(),
                description: if subagent.mode == SubAgentMode::Stateful {
                    format!(
                        "{}\n\nIMPORTANT: This sub-agent does NOT support concurrent invocation. Do NOT call this tool more than once in the same tool-call batch. If you need to invoke it multiple times, call it sequentially — one at a time.",
                        subagent.description
                    )
                } else {
                    subagent.description.to_string()
                },
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
