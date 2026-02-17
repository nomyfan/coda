use serde_json::Value;

#[derive(Debug, Clone)]
pub(crate) struct ToolDefinition {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameter_schema: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct SystemMessage(pub(crate) String);

#[derive(Debug, Clone)]
pub(crate) struct UserMessage(pub(crate) String);

/// A message representing a response from the AI, which may include tool calls.
#[derive(Debug, Clone, Default)]
pub(crate) struct AssistantMessage {
    pub(crate) content: String,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) usage: Option<CompletionUsage>,
}

/// A message representing a tool call from the AI.
#[derive(Debug, Clone)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct CompletionUsage {
    pub(crate) prompt_tokens: u32,
    pub(crate) completion_tokens: u32,
}

/// A message representing the result of a tool execution.
#[derive(Debug, Clone)]
pub(crate) struct ToolMessage {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) result: String,
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
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
pub(crate) struct LLMProviderConfig {
    pub(crate) api_key: String,
    pub(crate) base_url: String,
    pub(crate) stream: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ChatCompletionRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) tools: Vec<ToolDefinition>,
    pub(crate) max_completion_tokens: Option<u32>,
    pub(crate) temperature: Option<f32>,
}

#[derive(Debug, Clone)]
pub(crate) enum StreamError {
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

pub(crate) trait LLMProvider: Send + Sync + 'static {
    async fn stream(
        &self,
        request: ChatCompletionRequest,
        on_content: impl AsyncFnMut(String),
    ) -> Result<AssistantMessage, StreamError>;
}
