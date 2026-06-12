use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameter_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMessage(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage(pub String);

/// A message representing a response from the AI, which may include tool calls.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<CompletionUsage>,
    /// Whether LLM generation for this assistant message was interrupted by user abort
    /// before a normal completion was produced.
    #[serde(default)]
    pub aborted: bool,
}

/// A message representing a tool call from the AI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// The output of a tool execution: success or error.
///
/// The LLM request layer is responsible for formatting this into the string
/// content required by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolOutput {
    Ok(String),
    Err(String),
}

/// Records the approval/execution state of a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolCallOutcome {
    /// Executed automatically without requiring user approval.
    Auto,
    /// Suspended, then the caller instructed the agent to execute.
    Approved,
    /// Suspended, then the caller provided the result directly.
    Resolved,
    /// Suspended, then the caller rejected execution.
    Rejected { reason: Option<String> },
    /// Execution was interrupted by user abort.
    Aborted,
}

/// A message representing the result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMessage {
    pub id: String,
    pub name: String,
    pub output: ToolOutput,
    pub outcome: ToolCallOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// System message.
    System(SystemMessage),
    /// User message.
    User(UserMessage),
    /// A message representing a response from the AI, which may include tool calls.
    Assistant(AssistantMessage),
    /// A message representing the result of a tool execution.
    Tool(ToolMessage),
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone)]
pub struct LLMProviderConfig {
    pub api_key: String,
    pub base_url: String,
    /// Request token-usage statistics in the streaming response.
    pub include_usage: bool,
}

/// How hard a reasoning model should think. Provider-agnostic; the provider
/// maps it to its own wire representation. `None` is the "thinking off" state —
/// distinct from `reasoning_effort` being absent, which leaves the provider's
/// own default untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

#[derive(Debug, Clone, Default)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Reasoning effort for this request. Outer `None` leaves the provider
    /// default; `Some(ReasoningEffort::None)` explicitly turns thinking off.
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Clone)]
pub enum StreamError {
    /// Error occurred during streaming the response from the LLM.
    StreamingError(String),
    /// Error occurred while parsing the LLM's response.
    InvalidResponse(String),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::StreamingError(err) => write!(f, "Streaming error: {}", err),
            StreamError::InvalidResponse(err) => write!(f, "Invalid response: {}", err),
        }
    }
}

impl std::error::Error for StreamError {}

/// Events produced by `LLMProvider::stream`.
pub enum LLMStreamEvent {
    ContentChunk(String),
    /// A chunk of the model's reasoning / chain-of-thought text, from providers
    /// that expose a separate reasoning stream (e.g. DeepSeek).
    ReasoningChunk(String),
    Completed(AssistantMessage),
}

pub trait LLMProvider: Send + Sync + 'static {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_;
}

impl<P: LLMProvider> LLMProvider for std::sync::Arc<P> {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        (**self).stream(request)
    }
}
