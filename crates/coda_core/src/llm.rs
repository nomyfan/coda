use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Stable identity of a persisted message.
///
/// Always minted server-side; clients only ever read it. Values are UUID v4, so
/// they never collide in practice, but the storage constraint only requires
/// uniqueness *within a session* — that is what lets a session fork copy its
/// messages verbatim instead of re-minting every id and rewriting the
/// references that point at them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(Uuid);

impl MessageId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// The id as a plain UUID, for storage backends that have a native uuid
    /// column type.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for MessageId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

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

/// Identity of a turn: the id of the root user message that started it.
///
/// Reusing that id rather than minting a separate one keeps "what is a turn"
/// self-evident and avoids a second ordering scheme — turns are ordered by where
/// their user message sits in the root thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(MessageId);

impl TurnId {
    /// The tag as a plain UUID, for storage backends that have a native uuid
    /// column type.
    pub fn as_uuid(&self) -> Uuid {
        self.0.as_uuid()
    }
}

impl From<MessageId> for TurnId {
    fn from(message_id: MessageId) -> Self {
        Self(message_id)
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Which sub-agent invocation produced a message.
///
/// A composite key rather than the bare `call_id`, because a tool call id is
/// only guaranteed unique within one assistant message — some providers number
/// them per response. Pairing it with the parent assistant's `message_id` keeps
/// the edge unambiguous even when a provider reuses call ids across turns, and
/// that matters permanently: once the ambiguous form is persisted, the causal
/// link can't be recovered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageOrigin {
    /// The assistant message whose tool call started this.
    pub message_id: MessageId,
    /// Which tool call within that message.
    pub call_id: String,
}

impl MessageOrigin {
    /// Render the pair as one name, for the places that need a single string to
    /// identify this exact invocation — deriving a stateless sub-agent's thread
    /// id from it, and recording that derivation afterwards.
    pub fn derivation_key(&self) -> String {
        format!("{}:{}", self.message_id, self.call_id)
    }
}

/// A user-turn message whose content may include text and/or images.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub message_id: MessageId,
    /// Set on the message that opens a sub-agent thread's work, naming the
    /// parent-thread call that triggered it. `None` for a root user message,
    /// which nothing triggered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    pub parts: Vec<ContentPart>,
    /// When the user turn was created. Stamped by the constructors so every
    /// message carries a timestamp for the UI.
    pub created_at: jiff::Timestamp,
}

impl UserMessage {
    /// Construct a text-only message.
    ///
    /// The id is passed in rather than minted here: a root user message is built
    /// twice from the same task — once for the agent's persisted history and
    /// once for the relay's in-memory snapshot — and both copies must carry the
    /// same id.
    pub fn text(message_id: MessageId, text: impl Into<String>) -> Self {
        Self {
            message_id,
            origin: None,
            parts: vec![ContentPart::Text { text: text.into() }],
            created_at: jiff::Timestamp::now(),
        }
    }

    /// Construct the message that opens a sub-agent thread's work, recording
    /// which parent-thread call triggered it.
    pub fn from_subagent_call(
        message_id: MessageId,
        text: impl Into<String>,
        origin: MessageOrigin,
    ) -> Self {
        Self {
            origin: Some(origin),
            ..Self::text(message_id, text)
        }
    }

    /// Construct a message with optional text and zero or more image URLs
    /// (data-URIs or HTTPS URLs). An empty `text` produces a pure-image
    /// message with no text part, since some providers reject empty text parts.
    pub fn with_images(message_id: MessageId, text: impl Into<String>, images: &[String]) -> Self {
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
            message_id,
            origin: None,
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
    /// Minted where the message is constructed — the provider adapter for a
    /// normal completion, the runtime for an aborted one. Each object is built
    /// exactly once and then flows through the event pipeline unchanged, so the
    /// id the caller sees is always the id that gets persisted.
    pub message_id: MessageId,
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

/// Immutable presentation data produced by a tool execution. Artifacts are
/// persisted with the result message but are not sent to the model as output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolArtifact {
    FileDiff {
        path: String,
        operation: FileChangeOperation,
        patch: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeOperation {
    Create,
    Modify,
    Delete,
}

/// A message representing the result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMessage {
    pub message_id: MessageId,
    /// The id of the tool call this message answers. Distinct from
    /// `message_id`, which identifies this message itself.
    pub id: String,
    pub name: String,
    pub output: ToolOutput,
    pub outcome: ToolCallOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ToolArtifact>,
    /// When the tool call began executing, when known. Calls that resolve
    /// instantly (rejections, dispatch errors) leave this absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<jiff::Timestamp>,
    /// When the tool call produced this result. Paired with `started_at`, it
    /// yields the execution duration for the UI.
    pub ended_at: jiff::Timestamp,
}

impl ToolMessage {
    /// Construct a tool result, stamping `message_id` and `ended_at` at the
    /// current instant. Pass `started_at` when execution timing is known;
    /// instantaneous results (rejections, dispatch failures) pass `None`.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        output: ToolOutput,
        outcome: ToolCallOutcome,
        started_at: Option<jiff::Timestamp>,
    ) -> Self {
        Self {
            message_id: MessageId::new(),
            id: id.into(),
            name: name.into(),
            output,
            outcome,
            artifacts: Vec::new(),
            started_at,
            ended_at: jiff::Timestamp::now(),
        }
    }

    pub fn with_artifacts(mut self, artifacts: Vec<ToolArtifact>) -> Self {
        self.artifacts = artifacts;
        self
    }
}

/// A record the compaction machinery authored: the summary a successful
/// compaction leaves behind, or a note that one could not be produced.
///
/// `outcome` says which of the two this is — and, for a summary, where the
/// boundary it draws falls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionMessage {
    pub message_id: MessageId,
    pub outcome: CompactionOutcome,
    pub content: String,
    pub created_at: jiff::Timestamp,
}

impl CompactionMessage {
    /// The last message this summary covers, or `None` for a failure record.
    pub fn cutoff(&self) -> Option<MessageId> {
        match self.outcome {
            CompactionOutcome::Summary { cutoff } => Some(cutoff),
            CompactionOutcome::Failed => None,
        }
    }

    /// Whether this is a summary — the only outcome that moves the boundary.
    pub fn is_summary(&self) -> bool {
        matches!(self.outcome, CompactionOutcome::Summary { .. })
    }
}

/// What a compaction attempt produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompactionOutcome {
    /// A summary, which is also the new boundary. `cutoff` is the last message
    /// it covers, which is what lets the model view put messages written after
    /// it (an in-progress turn) back *after* the summary. Lowered to a user
    /// message on the provider path — no `Tool`, and no assistant either: a
    /// tool message needs the id of the call it answers, and one built without
    /// a matching `tool_calls` entry ahead of it is an orphan result that
    /// providers reject.
    Summary { cutoff: MessageId },
    /// No summary could be produced, so the history was left as it is. The
    /// record is transcript-only: the model view filters it out, so nothing
    /// ever lowers it.
    Failed,
}

/// What a thread's history holds.
///
/// No `System`: the system prompt is not history. It is prepended when a
/// request is built — see [`RequestMessage`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// User message.
    User(UserMessage),
    /// A message representing a response from the AI, which may include tool calls.
    Assistant(AssistantMessage),
    /// A message representing the result of a tool execution.
    Tool(ToolMessage),
    /// A record the compaction machinery authored — see [`CompactionMessage`].
    Compaction(CompactionMessage),
}

impl Message {
    /// The id this message carries. Total, so anything that has to name a
    /// message — a compaction cutoff, a rewind target — can name any of them.
    pub fn message_id(&self) -> MessageId {
        match self {
            Message::User(message) => message.message_id,
            Message::Assistant(message) => message.message_id,
            Message::Tool(message) => message.message_id,
            Message::Compaction(message) => message.message_id,
        }
    }

    /// Whether the model view shows this message. A failed compaction is
    /// transcript-only; everything else is ordinary conversation shown in
    /// full.
    pub fn visible_to_model(&self) -> bool {
        match self {
            Message::Compaction(compaction) => compaction.is_summary(),
            _ => true,
        }
    }
}

/// What a provider is sent.
///
/// No `Compaction`: a summary is lowered to an ordinary user message before
/// the request is built, so a provider adapter never has to know that
/// compaction exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RequestMessage {
    /// System message.
    System(SystemMessage),
    /// User message.
    User(UserMessage),
    /// A message representing a response from the AI, which may include tool calls.
    Assistant(AssistantMessage),
    /// A message representing the result of a tool execution.
    Tool(ToolMessage),
}

impl From<&Message> for Option<RequestMessage> {
    fn from(message: &Message) -> Self {
        match message {
            Message::User(message) => Some(RequestMessage::User(message.clone())),
            Message::Assistant(message) => Some(RequestMessage::Assistant(message.clone())),
            Message::Tool(message) => Some(RequestMessage::Tool(message.clone())),
            // The request vector is discarded after the call, so reusing the
            // compaction message's own id costs nothing. The `None` arm is not
            // a tripwire: a failed compaction is transcript-only by
            // definition, so skipping it at the lowering is the correct
            // behavior even if a caller forgot to filter the model view.
            Message::Compaction(message) => match message.outcome {
                CompactionOutcome::Summary { .. } => Some(RequestMessage::User(UserMessage::text(
                    message.message_id,
                    message.content.clone(),
                ))),
                CompactionOutcome::Failed => None,
            },
        }
    }
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
    pub messages: Vec<RequestMessage>,
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
            message_id: MessageId::new(),
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
            "message_id": MessageId::new(),
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
