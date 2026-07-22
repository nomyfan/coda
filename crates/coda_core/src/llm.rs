use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Opaque, provider-formatted reasoning state that must be replayed on a later
/// request. The format tag prevents one provider dialect from interpreting
/// another dialect's payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReasoningContinuation {
    format: String,
    payload: Value,
}

impl ReasoningContinuation {
    pub fn try_new(format: impl Into<String>, payload: Value) -> Result<Self, String> {
        let format = format.into();
        if format.trim().is_empty() {
            return Err("reasoning continuation format must not be empty".to_string());
        }
        let has_payload = match &payload {
            Value::Array(values) => !values.is_empty(),
            Value::Object(values) => !values.is_empty(),
            _ => false,
        };
        if !has_payload {
            return Err(
                "reasoning continuation payload must be a non-empty object or array".to_string(),
            );
        }
        Ok(Self { format, payload })
    }

    pub fn format(&self) -> &str {
        &self.format
    }

    pub fn payload_for(&self, format: &str) -> Option<&Value> {
        (self.format == format).then_some(&self.payload)
    }
}

impl<'de> Deserialize<'de> for ReasoningContinuation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireContinuation {
            format: String,
            payload: Value,
        }

        let value = WireContinuation::deserialize(deserializer)?;
        Self::try_new(value.format, value.payload).map_err(serde::de::Error::custom)
    }
}

/// Structured error returned by an upstream model provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    /// Static provider identifier configured by the Coda deployment.
    pub provider_id: String,
    /// HTTP status for a rejected request, or the equivalent status reported
    /// by an error envelope delivered after streaming has started.
    pub status_code: Option<u16>,
    /// Stable provider classification when one is available. For OpenRouter
    /// this maps from `error.metadata.error_type`.
    pub error_type: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameter_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMessage(pub String);

/// A single piece of multimodal user content: plain text or an image.
///
/// Images are passed as data URIs (`data:image/<fmt>;base64,<b64>`) or HTTPS
/// URLs. The provider receives them without a `detail` hint so it applies its
/// own default (equivalent to `"auto"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    Image { url: String },
}

/// A user-turn message whose content may include text and/or images.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub parts: Vec<ContentPart>,
    /// When the user turn was created. Stamped by the constructors so every
    /// message carries a timestamp for the UI.
    pub created_at: jiff::Timestamp,
}

impl UserMessage {
    /// Construct a text-only message.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            parts: vec![ContentPart::Text { text: text.into() }],
            created_at: jiff::Timestamp::now(),
        }
    }

    /// Construct a message with optional text and zero or more image URLs
    /// (data-URIs or HTTPS URLs). An empty `text` produces a pure-image
    /// message with no text part, since some providers reject empty text parts.
    pub fn with_images(text: impl Into<String>, images: &[String]) -> Self {
        let text = text.into();
        let mut parts = Vec::with_capacity(images.len() + 1);
        if !text.is_empty() {
            parts.push(ContentPart::Text { text });
        }
        parts.extend(
            images
                .iter()
                .map(|url| ContentPart::Image { url: url.clone() }),
        );
        Self {
            parts,
            created_at: jiff::Timestamp::now(),
        }
    }

    /// Return the first text part, used for session-list previews.
    pub fn first_text(&self) -> Option<&str> {
        self.parts.iter().find_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } => None,
        })
    }

    /// Whether the message carries at least one image part. Used to render a
    /// list preview for image-only turns that have no text.
    pub fn has_image(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, ContentPart::Image { .. }))
    }
}

/// A message representing a response from the AI, which may include tool calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<CompletionUsage>,
    /// Provider-specific reasoning captured separately from assistant content.
    /// Request adapters decide when the provider needs it on later turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Opaque provider state used only when replaying a reasoning tool-call
    /// turn. It is deliberately separate from user-visible reasoning text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_continuation: Option<ReasoningContinuation>,
    /// When the reasoning phase ended, when the provider streamed reasoning
    /// separately from answer content. This is distinct from `ended_at`, which
    /// covers the whole generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_ended_at: Option<jiff::Timestamp>,
    /// Whether LLM generation for this assistant message was interrupted by user abort
    /// before a normal completion was produced.
    #[serde(default)]
    pub aborted: bool,
    /// When generation started (the moment the request was dispatched). Set by
    /// the agent runtime, not the provider.
    pub started_at: jiff::Timestamp,
    /// When generation finished. Paired with `started_at`, it yields the
    /// model's generation duration for the UI.
    pub ended_at: jiff::Timestamp,
}

/// A message representing a tool call from the AI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Option<String>,
}

/// Provider-agnostic token usage persisted with an assistant message.
///
/// Provider adapters normalize their wire formats into this structure before
/// the message reaches the agent runtime or checkpoint storage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionUsage {
    /// Tokens supplied to the model for this request.
    pub prompt_tokens: u32,
    /// Tokens generated by the model for this request.
    pub completion_tokens: u32,
    /// Total tokens reported by the provider, normally prompt plus completion.
    pub total_tokens: u32,
    /// Optional prompt breakdown assembled from standard and provider extensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Optional completion breakdown from the standard OpenAI-compatible details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

/// Normalized prompt-token details across supported providers.
///
/// Standard OpenAI-compatible APIs report `audio_tokens` and `cached_tokens`
/// inside `prompt_tokens_details`. DeepSeek reports cache hit and miss counts as
/// top-level usage fields, which its adapter places in this same structure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    /// Audio input tokens reported by standard OpenAI-compatible APIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<u32>,
    /// Prompt tokens served from cache through the standard OpenAI details field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    /// Prompt tokens served from DeepSeek's cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_tokens: Option<u32>,
    /// Prompt tokens processed after missing DeepSeek's cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_miss_tokens: Option<u32>,
}

/// Normalized completion-token details from OpenAI-compatible providers.
///
/// Each field remains absent when the provider omits that metric.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionTokensDetails {
    /// Predicted-output tokens accepted into the generated completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_prediction_tokens: Option<u32>,
    /// Audio output tokens generated by the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<u32>,
    /// Internal reasoning tokens counted within completion tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    /// Predicted-output tokens rejected from the generated completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_prediction_tokens: Option<u32>,
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
    /// When the tool call began executing, when known. Calls that resolve
    /// instantly (rejections, dispatch errors) leave this absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<jiff::Timestamp>,
    /// When the tool call produced this result. Paired with `started_at`, it
    /// yields the execution duration for the UI.
    pub ended_at: jiff::Timestamp,
}

impl ToolMessage {
    /// Construct a tool result, stamping `ended_at` at the current instant.
    /// Pass `started_at` when execution timing is known; instantaneous results
    /// (rejections, dispatch failures) pass `None`.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        output: ToolOutput,
        outcome: ToolCallOutcome,
        started_at: Option<jiff::Timestamp>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            output,
            outcome,
            started_at,
            ended_at: jiff::Timestamp::now(),
        }
    }
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

/// An input modality a model can accept. Provider-agnostic. Every model accepts
/// `Text`; richer modalities (e.g. `Image`) are opt-in per model and gate the
/// corresponding UI affordances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Text,
    Image,
}

#[derive(Debug, Clone, Default)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Reasoning effort for this request. `None` leaves the provider default;
    /// `Some("off")` explicitly turns thinking off.
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub enum StreamError {
    /// Network transport or SSE framing failed while opening or consuming the
    /// provider stream.
    TransportError(String),
    /// The provider adapter could not construct a valid outbound request from
    /// the supplied request or persisted conversation state.
    InvalidRequest(String),
    /// A successful provider response could not be decoded or assembled into
    /// a valid completion.
    InvalidResponse(String),
    /// A structured error envelope returned by the provider, including errors
    /// delivered inside an otherwise successful streaming HTTP response.
    Provider(ProviderError),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::TransportError(err) => write!(f, "Transport error: {}", err),
            StreamError::InvalidRequest(err) => write!(f, "Invalid request: {}", err),
            StreamError::InvalidResponse(err) => write!(f, "Invalid response: {}", err),
            StreamError::Provider(err) => {
                write!(f, "Provider {} error", err.provider_id)?;
                if let Some(status_code) = err.status_code {
                    write!(f, " {status_code}")?;
                }
                if let Some(error_type) = &err.error_type {
                    write!(f, " ({error_type})")?;
                }
                write!(f, ": {}", err.message)
            }
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
    Completed(Box<AssistantMessage>),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_reasoning_roundtrips_and_defaults_when_absent() {
        let now = jiff::Timestamp::now();
        let message = AssistantMessage {
            content: String::new(),
            tool_calls: vec![],
            usage: None,
            reasoning_content: Some("tool reasoning".into()),
            reasoning_continuation: Some(
                ReasoningContinuation::try_new(
                    "openrouter.reasoning_details.v1",
                    serde_json::json!([{"type": "reasoning.encrypted", "data": "opaque"}]),
                )
                .unwrap(),
            ),
            reasoning_ended_at: None,
            aborted: false,
            started_at: now,
            ended_at: now,
        };
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(
            value["reasoning_content"],
            serde_json::json!("tool reasoning")
        );
        assert_eq!(
            value["reasoning_continuation"]["format"],
            serde_json::json!("openrouter.reasoning_details.v1")
        );

        let roundtripped: AssistantMessage = serde_json::from_value(value).unwrap();
        assert_eq!(
            roundtripped
                .reasoning_continuation
                .as_ref()
                .and_then(|continuation| {
                    continuation.payload_for("openrouter.reasoning_details.v1")
                }),
            Some(&serde_json::json!([{
                "type": "reasoning.encrypted",
                "data": "opaque"
            }]))
        );

        let now = jiff::Timestamp::now();
        let without_reasoning: AssistantMessage = serde_json::from_value(serde_json::json!({
            "content": "",
            "tool_calls": [],
            "usage": null,
            "aborted": false,
            "started_at": now,
            "ended_at": now,
        }))
        .unwrap();
        assert!(without_reasoning.reasoning_content.is_none());
        assert!(without_reasoning.reasoning_continuation.is_none());
    }

    #[test]
    fn reasoning_continuation_rejects_invalid_envelopes() {
        assert!(ReasoningContinuation::try_new("", serde_json::json!([{}])).is_err());
        assert!(
            ReasoningContinuation::try_new("openrouter.reasoning_details.v1", Value::Null).is_err()
        );
        assert!(
            serde_json::from_value::<ReasoningContinuation>(serde_json::json!({
                "format": "openrouter.reasoning_details.v1",
                "payload": []
            }))
            .is_err()
        );
    }

    #[test]
    fn provider_error_display_keeps_structured_context() {
        let error = StreamError::Provider(ProviderError {
            provider_id: "openrouter".into(),
            status_code: Some(429),
            error_type: Some("rate_limit_exceeded".into()),
            message: "slow down".into(),
        });

        assert_eq!(
            error.to_string(),
            "Provider openrouter error 429 (rate_limit_exceeded): slow down"
        );
    }

    #[test]
    fn invalid_request_display_identifies_the_outbound_boundary() {
        let error = StreamError::InvalidRequest("continuation is malformed".into());

        assert_eq!(
            error.to_string(),
            "Invalid request: continuation is malformed"
        );
    }
}
