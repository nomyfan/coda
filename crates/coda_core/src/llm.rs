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

/// Who produced a user-turn message: the human, or the runtime delivering
/// background-task completion notices on their behalf. Persisted with the
/// message; clients render notices differently, and restore paths use the
/// carried task ids to dedupe re-delivery.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserOrigin {
    #[default]
    Human,
    /// The stable notice fact keys this message delivered — the dedupe keys for
    /// restore. A task's completion and its later output-expiration are
    /// *different* facts with different keys, so re-delivering one never
    /// suppresses the other.
    TaskNotice { notice_keys: Vec<TaskNoticeKey> },
}

/// Stable identity of a background-task notice *fact*, independent of its
/// mutable payload (status, timestamp, reason). Lives here (rather than in
/// `coda_tools`) so wire/history types can carry it without a reverse crate
/// dependency; it is identity-only and never used to build an archive path,
/// hence the plain `String` task id.
#[derive(Debug, Clone, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskNoticeKey {
    /// A task reached a terminal state.
    Completed { task_id: String },
    /// A task's retained output was later evicted by the session quota.
    OutputExpired { task_id: String },
    /// One overflow aggregate, identified by a stable id minted at creation so
    /// facts it cannot enumerate individually are still deduped as a batch.
    OverflowBatch { batch_id: String },
}

/// A user-turn message whose content may include text and/or images.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub parts: Vec<ContentPart>,
    /// When the user turn was created. Stamped by the constructors so every
    /// message carries a timestamp for the UI.
    pub created_at: jiff::Timestamp,
    /// Defaults to `Human`, so checkpoints written before this field existed
    /// deserialize as ordinary user messages.
    #[serde(default)]
    pub origin: UserOrigin,
}

impl UserMessage {
    /// Construct a text-only message.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            parts: vec![ContentPart::Text { text: text.into() }],
            created_at: jiff::Timestamp::now(),
            origin: UserOrigin::Human,
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
            origin: UserOrigin::Human,
        }
    }

    /// Construct a background-task notice delivered as a user-turn message,
    /// carrying the covered notice fact keys for restore-time dedupe.
    pub fn task_notice(text: impl Into<String>, notice_keys: Vec<TaskNoticeKey>) -> Self {
        Self {
            parts: vec![ContentPart::Text { text: text.into() }],
            created_at: jiff::Timestamp::now(),
            origin: UserOrigin::TaskNotice { notice_keys },
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
    }
}
