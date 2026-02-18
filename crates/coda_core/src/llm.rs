use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameter_schema: Value,
}

#[derive(Debug, Clone)]
pub struct SystemMessage(pub String);

#[derive(Debug, Clone)]
pub struct UserMessage(pub String);

/// A message representing a response from the AI, which may include tool calls.
#[derive(Debug, Clone, Default)]
pub struct AssistantMessage {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<CompletionUsage>,
}

/// A message representing a tool call from the AI.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompletionUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// A message representing the result of a tool execution.
#[derive(Debug, Clone)]
pub struct ToolMessage {
    pub id: String,
    pub name: String,
    pub result: String,
}

#[derive(Debug, Clone)]
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
    pub stream: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
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

pub trait LLMProvider: Send + Sync + 'static {
    fn stream(
        &self,
        request: ChatCompletionRequest,
        on_content: impl AsyncFnMut(String),
    ) -> impl std::future::Future<Output = Result<AssistantMessage, StreamError>>;
}
