use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCallChunk,
    ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessage,
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestAssistantMessageContentPart,
    ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartImage,
    ChatCompletionRequestMessageContentPartText, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
    ChatCompletionStreamOptions, ChatCompletionTool, ChatCompletionTools,
    CompletionTokensDetails as OpenAICompletionTokensDetails, CreateChatCompletionRequestArgs,
    FunctionCall, FunctionCallStream, FunctionObject, ImageUrl,
    PromptTokensDetails as OpenAIPromptTokensDetails,
};
use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, CompletionTokensDetails, CompletionUsage, ContentPart,
    LLMProvider, LLMProviderConfig, LLMStreamEvent, Message, PromptTokensDetails, ProviderError,
    ReasoningContinuation, StreamError, ToolCall, ToolCallOutcome, ToolDefinition, ToolOutput,
};
use futures::{Stream, StreamExt};

trait IntoOpenAIType<T> {
    fn into_openai_type(self) -> T;
}

impl IntoOpenAIType<ChatCompletionRequestMessage> for Message {
    fn into_openai_type(self) -> ChatCompletionRequestMessage {
        match self {
            Message::System(system_message) => {
                //
                ChatCompletionRequestSystemMessage {
                    content: ChatCompletionRequestSystemMessageContent::Text(system_message.0),
                    ..Default::default()
                }
                .into()
            }
            Message::User(user_message) => {
                let has_images = user_message
                    .parts
                    .iter()
                    .any(|p| matches!(p, ContentPart::Image { .. }));
                let content = if has_images {
                    let parts = user_message
                        .parts
                        .into_iter()
                        .map(|p| match p {
                            ContentPart::Text { text } => {
                                ChatCompletionRequestUserMessageContentPart::Text(
                                    ChatCompletionRequestMessageContentPartText { text },
                                )
                            }
                            ContentPart::Image { url } => {
                                ChatCompletionRequestUserMessageContentPart::ImageUrl(
                                    ChatCompletionRequestMessageContentPartImage {
                                        image_url: ImageUrl { url, detail: None },
                                    },
                                )
                            }
                        })
                        .collect();
                    ChatCompletionRequestUserMessageContent::Array(parts)
                } else {
                    let text = user_message
                        .parts
                        .into_iter()
                        .filter_map(|p| match p {
                            ContentPart::Text { text } => Some(text),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    ChatCompletionRequestUserMessageContent::Text(text)
                };
                ChatCompletionRequestUserMessage {
                    content,
                    ..Default::default()
                }
                .into()
            }
            Message::Assistant(assistant_message) => {
                let content = if assistant_message.aborted {
                    // Aborted messages use array form so the abort marker is a
                    // separate content part the model won't confuse with normal output.
                    let mut parts = vec![];
                    if !assistant_message.content.is_empty() {
                        parts.push(ChatCompletionRequestAssistantMessageContentPart::Text(
                            ChatCompletionRequestMessageContentPartText {
                                text: assistant_message.content,
                            },
                        ));
                    }
                    parts.push(ChatCompletionRequestAssistantMessageContentPart::Text(
                        ChatCompletionRequestMessageContentPartText {
                            text: "Error: Aborted by user".to_string(),
                        },
                    ));
                    Some(ChatCompletionRequestAssistantMessageContent::Array(parts))
                } else {
                    Some(ChatCompletionRequestAssistantMessageContent::Text(
                        assistant_message.content,
                    ))
                };

                let tool_calls: Vec<ChatCompletionMessageToolCalls> = assistant_message
                    .tool_calls
                    .into_iter()
                    .map(|x| {
                        ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                            id: x.id,
                            function: FunctionCall {
                                name: x.name,
                                arguments: if let Some(arguments) = x.arguments {
                                    arguments.clone()
                                } else {
                                    "".to_string()
                                },
                            },
                        })
                    })
                    .collect();

                ChatCompletionRequestAssistantMessage {
                    content,
                    // Omit the field entirely when empty: some providers (e.g. DeepSeek)
                    // reject `"tool_calls": []` with a 400.
                    tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                    ..Default::default()
                }
                .into()
            }
            Message::Tool(tool_message) => {
                let content = match tool_message.outcome {
                    ToolCallOutcome::Rejected { reason } => match reason {
                        Some(r) => format!("[REJECTED BY USER] {r}"),
                        None => "[REJECTED BY USER]".to_string(),
                    },
                    ToolCallOutcome::Aborted => "[ABORTED BY USER]".to_string(),
                    _ => match tool_message.output {
                        ToolOutput::Ok(s) => s,
                        ToolOutput::Err(s) => format!("Error: {s}"),
                    },
                };
                ChatCompletionRequestToolMessage {
                    tool_call_id: tool_message.id,
                    content: ChatCompletionRequestToolMessageContent::Text(content),
                }
                .into()
            }
        }
    }
}

impl IntoOpenAIType<ChatCompletionTools> for ToolDefinition {
    fn into_openai_type(self) -> ChatCompletionTools {
        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: self.name,
                description: Some(self.description),
                parameters: Some(self.parameter_schema),
                // Strict mode requires every property to be listed in `required` and
                // `additionalProperties: false`. Arbitrary tool schemas (notably MCP
                // tools) routinely violate this, so leave it off rather than reject them.
                strict: None,
            },
        })
    }
}

/// Streaming response superset accepted from supported OpenAI-compatible APIs.
#[derive(Debug, serde::Deserialize)]
struct CompatibleStreamResponse {
    #[serde(default)]
    choices: Vec<CompatibleStreamChoice>,
    usage: Option<ProviderCompletionUsage>,
    error: Option<CompatibleProviderError>,
}

#[derive(Debug, serde::Deserialize)]
struct CompatibleProviderError {
    code: Option<u16>,
    message: String,
    metadata: Option<CompatibleErrorMetadata>,
}

#[derive(Debug, serde::Deserialize)]
struct CompatibleErrorMetadata {
    error_type: Option<String>,
}

impl CompatibleProviderError {
    fn into_provider_error(self, provider_id: &str) -> ProviderError {
        ProviderError {
            provider_id: provider_id.to_string(),
            status_code: self.code,
            error_type: self.metadata.and_then(|metadata| metadata.error_type),
            message: self.message,
        }
    }
}

fn provider_api_error(
    provider_id: &str,
    response: async_openai::error::ApiErrorResponse,
) -> ProviderError {
    ProviderError {
        provider_id: provider_id.to_string(),
        status_code: Some(response.status_code.as_u16()),
        error_type: response.api_error.r#type,
        message: response.api_error.message,
    }
}

fn provider_error_from_raw_body(
    kind: ProviderKind,
    provider_id: &str,
    raw: String,
) -> ProviderError {
    if kind == ProviderKind::OpenRouter
        && let Ok(response) = serde_json::from_str::<CompatibleStreamResponse>(&raw)
        && let Some(error) = response.error
    {
        return error.into_provider_error(provider_id);
    }

    let envelope = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|body| body.get("error").cloned());
    let error_type = envelope
        .as_ref()
        .and_then(|error| error.get("type"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let message = envelope
        .as_ref()
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or(raw);

    ProviderError {
        provider_id: provider_id.to_string(),
        // async-openai does not retain the HTTP status when deserializing the
        // provider's non-success body fails. Do not reinterpret a native
        // provider `error.code` string as an HTTP status.
        status_code: None,
        error_type,
        message,
    }
}

fn map_request_error(kind: ProviderKind, provider_id: &str, error: OpenAIError) -> StreamError {
    match error {
        OpenAIError::ApiError(response) => {
            StreamError::Provider(provider_api_error(provider_id, response))
        }
        // At stream creation, JSON deserialization applies to the non-success
        // response body. It is therefore a provider rejection even when its
        // envelope does not match async-openai's OpenAI error schema.
        OpenAIError::JSONDeserialize(_, raw) => {
            StreamError::Provider(provider_error_from_raw_body(kind, provider_id, raw))
        }
        OpenAIError::InvalidArgument(message) => StreamError::InvalidRequest(message),
        other => StreamError::TransportError(other.to_string()),
    }
}

fn map_stream_error(provider_id: &str, error: OpenAIError) -> StreamError {
    match error {
        OpenAIError::ApiError(response) => {
            StreamError::Provider(provider_api_error(provider_id, response))
        }
        OpenAIError::JSONDeserialize(error, raw) => StreamError::InvalidResponse(format!(
            "failed to decode provider SSE event: {error}; payload: {raw}"
        )),
        OpenAIError::InvalidArgument(message) => StreamError::InvalidRequest(message),
        other => StreamError::TransportError(other.to_string()),
    }
}

/// Superset of usage fields returned by supported OpenAI-compatible providers.
///
/// Standard OpenAI details use the nested `*_tokens_details` objects. DeepSeek
/// additionally reports cache hit and miss counts as top-level fields. The
/// normalization step selects provider-specific fields through [`ProviderKind`].
#[derive(Debug, serde::Deserialize)]
struct ProviderCompletionUsage {
    /// Tokens sent to the model, including cached and provider-managed prompt tokens.
    prompt_tokens: u32,
    /// Tokens generated by the model, including reasoning tokens when reported.
    completion_tokens: u32,
    /// Provider-reported total. Some compatible APIs omit it or return zero.
    #[serde(default)]
    total_tokens: u32,
    /// Standard OpenAI prompt-token breakdown.
    prompt_tokens_details: Option<OpenAIPromptTokensDetails>,
    /// Standard OpenAI completion-token breakdown.
    completion_tokens_details: Option<OpenAICompletionTokensDetails>,
    /// DeepSeek-specific top-level count of prompt tokens served from cache.
    prompt_cache_hit_tokens: Option<u32>,
    /// DeepSeek-specific top-level count of prompt tokens that missed cache.
    prompt_cache_miss_tokens: Option<u32>,
}

impl ProviderCompletionUsage {
    /// Normalize provider wire usage into Coda's provider-agnostic usage model.
    fn into_completion_usage(self, kind: ProviderKind) -> CompletionUsage {
        let prompt_details = self.prompt_tokens_details.unwrap_or_default();
        let (cache_hit_tokens, cache_miss_tokens) = match kind {
            ProviderKind::Generic | ProviderKind::OpenRouter => (None, None),
            ProviderKind::Deepseek => (self.prompt_cache_hit_tokens, self.prompt_cache_miss_tokens),
        };
        let prompt_tokens_details = PromptTokensDetails {
            audio_tokens: prompt_details.audio_tokens,
            cached_tokens: prompt_details.cached_tokens,
            cache_hit_tokens,
            cache_miss_tokens,
        };
        let completion_tokens_details =
            self.completion_tokens_details
                .map(|details| CompletionTokensDetails {
                    accepted_prediction_tokens: details.accepted_prediction_tokens,
                    audio_tokens: details.audio_tokens,
                    reasoning_tokens: details.reasoning_tokens,
                    rejected_prediction_tokens: details.rejected_prediction_tokens,
                });
        CompletionUsage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: if self.total_tokens == 0 {
                self.prompt_tokens + self.completion_tokens
            } else {
                self.total_tokens
            },
            prompt_tokens_details: (prompt_tokens_details != PromptTokensDetails::default())
                .then_some(prompt_tokens_details),
            completion_tokens_details,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct CompatibleStreamChoice {
    delta: CompatibleStreamDelta,
}

#[derive(Debug, serde::Deserialize)]
struct CompatibleStreamDelta {
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    reasoning_details: Option<Vec<serde_json::Value>>,
    tool_calls: Option<Vec<ChatCompletionMessageToolCallChunk>>,
}

/// Which OpenAI-compatible API the provider talks to. Most knobs are shared,
/// but the way thinking is toggled differs: DeepSeek takes a top-level
/// `thinking` object, while standard OpenAI-compatible endpoints rely on
/// `reasoning_effort` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderKind {
    #[default]
    Generic,
    Deepseek,
    OpenRouter,
}

fn inject_deepseek_reasoning(body: &mut serde_json::Value, messages: &[Message]) {
    let Some(wire_messages) = body
        .get_mut("messages")
        .and_then(|value| value.as_array_mut())
    else {
        return;
    };
    for (wire_message, message) in wire_messages.iter_mut().zip(messages) {
        let Message::Assistant(assistant) = message else {
            continue;
        };
        if assistant.tool_calls.is_empty() {
            continue;
        }
        let Some(reasoning) = assistant.reasoning_content.as_ref() else {
            continue;
        };
        if let Some(object) = wire_message.as_object_mut() {
            object.insert(
                "reasoning_content".to_string(),
                serde_json::Value::String(reasoning.clone()),
            );
        }
    }
}

const OPENROUTER_REASONING_DETAILS_FORMAT: &str = "openrouter.reasoning_details.v1";

fn inject_openrouter_reasoning(
    body: &mut serde_json::Value,
    messages: &[Message],
) -> Result<(), StreamError> {
    let Some(wire_messages) = body
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Err(StreamError::InvalidRequest(
            "encoded request is missing messages".to_string(),
        ));
    };
    for (wire_message, message) in wire_messages.iter_mut().zip(messages) {
        let Message::Assistant(assistant) = message else {
            continue;
        };
        if assistant.tool_calls.is_empty() {
            continue;
        }
        let Some(object) = wire_message.as_object_mut() else {
            return Err(StreamError::InvalidRequest(
                "encoded assistant message is not an object".to_string(),
            ));
        };
        if let Some(continuation) = &assistant.reasoning_continuation {
            let Some(payload) = continuation.payload_for(OPENROUTER_REASONING_DETAILS_FORMAT)
            else {
                // Preserve unknown continuation formats in history, but never
                // interpret or forward them through this dialect.
                continue;
            };
            let serde_json::Value::Array(details) = payload else {
                return Err(StreamError::InvalidRequest(
                    "OpenRouter reasoning continuation payload must be an array".to_string(),
                ));
            };
            if details.is_empty() || !details.iter().all(serde_json::Value::is_object) {
                return Err(StreamError::InvalidRequest(
                    "OpenRouter reasoning_details must be a non-empty array of objects".to_string(),
                ));
            }
            object.insert(
                "reasoning_details".to_string(),
                serde_json::Value::Array(details.clone()),
            );
        } else if let Some(reasoning) = &assistant.reasoning_content {
            object.insert(
                "reasoning".to_string(),
                serde_json::Value::String(reasoning.clone()),
            );
        }
    }
    Ok(())
}

impl ProviderKind {
    fn encode_request(
        self,
        request: ChatCompletionRequest,
        include_usage: bool,
    ) -> Result<serde_json::Value, StreamError> {
        let source_messages = request.messages.clone();
        let reasoning_effort = request.reasoning_effort.clone();
        let messages: Vec<ChatCompletionRequestMessage> = request
            .messages
            .into_iter()
            .map(IntoOpenAIType::into_openai_type)
            .collect();
        let tools: Vec<ChatCompletionTools> = request
            .tools
            .into_iter()
            .map(IntoOpenAIType::into_openai_type)
            .collect();
        let mut builder = CreateChatCompletionRequestArgs::default();
        builder.model(request.model).messages(messages).tools(tools);
        if let Some(temperature) = request.temperature {
            builder.temperature(temperature);
        }
        if let Some(max_completion_tokens) = request.max_completion_tokens {
            builder.max_completion_tokens(max_completion_tokens);
        }
        let mut encoded = builder
            .build()
            .map_err(|error| StreamError::InvalidRequest(error.to_string()))?;
        // BYOT does not inject this flag automatically.
        encoded.stream = Some(true);
        if include_usage {
            encoded.stream_options = Some(ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            });
        }
        let mut body = serde_json::to_value(encoded)
            .map_err(|error| StreamError::InvalidRequest(error.to_string()))?;

        match self {
            Self::Generic => {
                if let Some(effort) = reasoning_effort {
                    body["reasoning_effort"] = serde_json::Value::String(effort);
                }
            }
            Self::Deepseek => {
                inject_deepseek_reasoning(&mut body, &source_messages);
                if let Some(effort) = reasoning_effort {
                    if effort != "off" {
                        body["reasoning_effort"] = serde_json::Value::String(effort.clone());
                    }
                    body["thinking"] = serde_json::json!({
                        "type": if effort == "off" { "disabled" } else { "enabled" }
                    });
                }
            }
            Self::OpenRouter => {
                inject_openrouter_reasoning(&mut body, &source_messages)?;
                if let Some(effort) = reasoning_effort {
                    body["reasoning"] = serde_json::json!({
                        "effort": if effort == "off" { "none" } else { effort.as_str() }
                    });
                }
            }
        }
        Ok(body)
    }

    fn reduce_response(
        self,
        provider_id: &str,
        response: CompatibleStreamResponse,
        completion: &mut CompletionAccumulator,
    ) -> Result<Vec<LLMStreamEvent>, StreamError> {
        if let Some(error) = response.error {
            return Err(StreamError::Provider(
                error.into_provider_error(provider_id),
            ));
        }
        if let Some(usage) = response.usage {
            completion.usage = Some(usage.into_completion_usage(self));
        }
        let Some(choice) = response.choices.into_iter().next() else {
            return Ok(Vec::new());
        };
        let delta = choice.delta;
        let details = delta.reasoning_details.unwrap_or_default();
        if self == Self::OpenRouter {
            if !details.iter().all(serde_json::Value::is_object) {
                return Err(StreamError::InvalidResponse(
                    "OpenRouter reasoning_details contains a non-object value".to_string(),
                ));
            }
            completion.reasoning_details.extend(details.iter().cloned());
        }

        let reasoning = match self {
            Self::OpenRouter => delta
                .reasoning
                .or(delta.reasoning_content)
                .or_else(|| visible_reasoning_from_details(&details)),
            Self::Generic | Self::Deepseek => delta.reasoning_content,
        };
        let mut events = Vec::new();
        if let Some(reasoning) = reasoning.filter(|value| !value.is_empty()) {
            completion.reduce_reasoning(&reasoning);
            events.push(LLMStreamEvent::ReasoningChunk(reasoning));
        }
        if let Some(content) = delta.content.filter(|value| !value.is_empty()) {
            completion.reduce_content(&content);
            events.push(LLMStreamEvent::ContentChunk(content));
        }
        if let Some(calls) = delta.tool_calls {
            for chunk in calls {
                completion.reduce_tool_chunk(chunk);
            }
        }
        Ok(events)
    }
}

fn visible_reasoning_from_details(details: &[serde_json::Value]) -> Option<String> {
    let text = details
        .iter()
        .filter_map(|detail| {
            detail
                .get("text")
                .or_else(|| detail.get("summary"))
                .and_then(serde_json::Value::as_str)
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

#[derive(Debug, Clone)]
pub struct OpenAICompatible {
    client: Client<OpenAIConfig>,
    kind: ProviderKind,
    provider_id: String,
    /// Add `stream_options.include_usage` to the request body.
    include_usage: bool,
}

impl LLMProvider for OpenAICompatible {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        let kind = self.kind;
        let provider_id = self.provider_id.as_str();
        let include_usage = self.include_usage;
        async_stream::stream! {
            let body = match kind.encode_request(request, include_usage) {
                Ok(body) => body,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            let mut stream = match self.client.chat()
                .create_stream_byot::<_, CompatibleStreamResponse>(body)
                .await
            {
                Ok(stream) => stream,
                // Non-2xx responses surface here. Preserve raw compatible-provider
                // errors, and recover OpenRouter's structured envelope when the SDK
                // rejects fields such as its numeric `code`.
                Err(e) => {
                    yield Err(map_request_error(kind, provider_id, e));
                    return;
                }
            };
            let mut chat_completion = CompletionAccumulator::new();
            while let Some(response) = stream.next().await {
                match response {
                    Ok(stream_response) => {
                        match kind.reduce_response(provider_id, stream_response, &mut chat_completion) {
                            Ok(events) => {
                                for event in events {
                                    yield Ok(event);
                                }
                            }
                            Err(error) => {
                                yield Err(error);
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(map_stream_error(provider_id, e));
                        return;
                    }
                }
            }
            match chat_completion.try_into() {
                Ok(msg) => yield Ok(LLMStreamEvent::Completed(Box::new(msg))),
                Err(e) => yield Err(StreamError::InvalidResponse(e)),
            }
        }
    }
}

impl OpenAICompatible {
    pub fn new(
        config: LLMProviderConfig,
        kind: ProviderKind,
        provider_id: impl Into<String>,
    ) -> Self {
        let client_config = OpenAIConfig::new()
            .with_api_base(config.base_url)
            .with_api_key(config.api_key);
        Self {
            client: Client::with_config(client_config),
            kind,
            provider_id: provider_id.into(),
            include_usage: config.include_usage,
        }
    }
}

#[derive(Debug)]
struct CompletionAccumulator {
    content: String,
    reasoning_content: String,
    reasoning_details: Vec<serde_json::Value>,
    chunks: Vec<ChatCompletionMessageToolCallChunk>,
    usage: Option<CompletionUsage>,
    /// When this completion began streaming. Stamped at construction so the
    /// assembled `AssistantMessage` carries a real start time even before the
    /// agent runtime overlays its own dispatch/completion timing.
    started_at: jiff::Timestamp,
}

impl CompletionAccumulator {
    fn new() -> Self {
        CompletionAccumulator {
            content: String::new(),
            reasoning_content: String::new(),
            reasoning_details: Vec::new(),
            chunks: vec![],
            usage: None,
            started_at: jiff::Timestamp::now(),
        }
    }

    fn reduce_content(&mut self, content: &str) {
        self.content += content;
    }

    fn reduce_reasoning(&mut self, content: &str) {
        self.reasoning_content += content;
    }

    fn reduce_tool_chunk(&mut self, chunk: ChatCompletionMessageToolCallChunk) {
        let Some(existing) = self
            .chunks
            .iter_mut()
            .find(|existing| existing.index == chunk.index)
        else {
            self.chunks.push(chunk);
            return;
        };
        if let Some(id) = chunk.id {
            existing.id = Some(id);
        }
        let Some(fragment) = chunk.function else {
            return;
        };
        let function = existing.function.get_or_insert(FunctionCallStream {
            name: None,
            arguments: None,
        });
        if let Some(name) = fragment.name {
            function
                .name
                .get_or_insert_with(String::new)
                .push_str(&name);
        }
        if let Some(arguments) = fragment.arguments {
            function
                .arguments
                .get_or_insert_with(String::new)
                .push_str(&arguments);
        }
    }
}

impl TryFrom<CompletionAccumulator> for AssistantMessage {
    type Error = String;

    fn try_from(value: CompletionAccumulator) -> Result<Self, Self::Error> {
        let mut tool_calls = vec![];
        for chunk in value.chunks {
            let id = chunk.id.ok_or_else(|| "Missing tool call id".to_string())?;
            let function = chunk
                .function
                .ok_or_else(|| "Missing function call".to_string())?;
            let name = function
                .name
                .ok_or_else(|| "Missing function call name".to_string())?;
            let arguments = function.arguments;
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        let reasoning_content =
            (!value.reasoning_content.is_empty()).then_some(value.reasoning_content);
        let reasoning_continuation = if value.reasoning_details.is_empty() {
            None
        } else {
            Some(ReasoningContinuation::try_new(
                OPENROUTER_REASONING_DETAILS_FORMAT,
                serde_json::Value::Array(value.reasoning_details),
            )?)
        };
        if value.content.is_empty() && reasoning_content.is_none() && tool_calls.is_empty() {
            return Err("stream completed without content, reasoning, or tool calls".to_string());
        }
        Ok(AssistantMessage {
            content: value.content,
            tool_calls,
            usage: value.usage,
            reasoning_content,
            reasoning_continuation,
            reasoning_ended_at: None,
            aborted: false,
            // Real stream timing; the agent runtime later overlays its own
            // dispatch/completion timestamps, which bracket the whole request.
            started_at: value.started_at,
            ended_at: jiff::Timestamp::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROK_FIXTURE: &str = include_str!("../tests/fixtures/openrouter-grok-4.5.json");
    const KIMI_FIXTURE: &str = include_str!("../tests/fixtures/openrouter-kimi-k3.json");
    const GLM_FIXTURE: &str = include_str!("../tests/fixtures/openrouter-glm-5.2.json");

    /// Base assistant message for tests; callers override the fields they care
    /// about with struct-update syntax (`..assistant()`).
    fn assistant() -> AssistantMessage {
        let now = jiff::Timestamp::now();
        AssistantMessage {
            content: String::new(),
            tool_calls: vec![],
            usage: None,
            reasoning_content: None,
            reasoning_continuation: None,
            reasoning_ended_at: None,
            aborted: false,
            started_at: now,
            ended_at: now,
        }
    }

    #[test]
    fn user_text_message_uses_text_content_form() {
        let message: ChatCompletionRequestMessage =
            Message::User(coda_core::llm::UserMessage::text("hello")).into_openai_type();

        let ChatCompletionRequestMessage::User(user) = message else {
            panic!("expected user message");
        };
        assert!(matches!(
            user.content,
            ChatCompletionRequestUserMessageContent::Text(text) if text == "hello"
        ));
    }

    #[test]
    fn user_image_message_uses_array_content_form() {
        let image_url = "data:image/png;base64,abc123".to_string();
        let message: ChatCompletionRequestMessage = Message::User(
            coda_core::llm::UserMessage::with_images("look", std::slice::from_ref(&image_url)),
        )
        .into_openai_type();

        let ChatCompletionRequestMessage::User(user) = message else {
            panic!("expected user message");
        };
        let ChatCompletionRequestUserMessageContent::Array(parts) = user.content else {
            panic!("expected array content");
        };

        assert_eq!(parts.len(), 2);
        assert!(matches!(
            &parts[0],
            ChatCompletionRequestUserMessageContentPart::Text(text) if text.text == "look"
        ));
        assert!(matches!(
            &parts[1],
            ChatCompletionRequestUserMessageContentPart::ImageUrl(image)
                if image.image_url.url == image_url && image.image_url.detail.is_none()
        ));
    }

    #[test]
    fn injects_reasoning_only_for_assistant_tool_calls() {
        let messages = vec![
            Message::Assistant(AssistantMessage {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "shell".into(),
                    arguments: Some("{}".into()),
                }],
                reasoning_content: Some("need a tool".into()),
                ..assistant()
            }),
            Message::Assistant(AssistantMessage {
                content: "done".into(),
                reasoning_content: Some("final reasoning".into()),
                ..assistant()
            }),
        ];
        let mut body = serde_json::json!({
            "messages": [
                {"role": "assistant", "tool_calls": [{}]},
                {"role": "assistant", "content": "done"}
            ]
        });

        inject_deepseek_reasoning(&mut body, &messages);

        assert_eq!(
            body["messages"][0]["reasoning_content"],
            serde_json::json!("need a tool")
        );
        assert!(body["messages"][1].get("reasoning_content").is_none());
    }

    #[test]
    fn reduced_completion_keeps_reasoning_content() {
        let mut completion = CompletionAccumulator::new();
        completion.reduce_reasoning("first ");
        completion.reduce_reasoning("second");
        completion.reduce_tool_chunk(ChatCompletionMessageToolCallChunk {
            index: 0,
            id: Some("call-1".into()),
            r#type: None,
            function: Some(FunctionCallStream {
                name: Some("shell".into()),
                arguments: Some("{}".into()),
            }),
        });

        let message = AssistantMessage::try_from(completion).unwrap();

        assert_eq!(message.reasoning_content.as_deref(), Some("first second"));
    }

    #[test]
    fn reduced_completion_keeps_reasoning_without_tool_calls() {
        let mut completion = CompletionAccumulator::new();
        completion.reduce_reasoning("final reasoning");

        let message = AssistantMessage::try_from(completion).unwrap();

        assert_eq!(
            message.reasoning_content.as_deref(),
            Some("final reasoning")
        );
    }

    #[test]
    fn stream_usage_option_serializes_in_request_body() {
        let request = CreateChatCompletionRequestArgs::default()
            .model("test-model")
            .messages(Vec::<ChatCompletionRequestMessage>::new())
            .stream(true)
            .stream_options(ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            })
            .build()
            .unwrap();

        let body = serde_json::to_value(request).unwrap();

        assert_eq!(
            body["stream_options"]["include_usage"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn deepseek_usage_keeps_standard_and_cache_details() {
        let usage: ProviderCompletionUsage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 120,
            "completion_tokens": 30,
            "total_tokens": 150,
            "prompt_tokens_details": {
                "audio_tokens": 4,
                "cached_tokens": 80
            },
            "completion_tokens_details": {
                "accepted_prediction_tokens": 2,
                "audio_tokens": 3,
                "reasoning_tokens": 20,
                "rejected_prediction_tokens": 1
            },
            "prompt_cache_hit_tokens": 75,
            "prompt_cache_miss_tokens": 45
        }))
        .unwrap();

        let usage = usage.into_completion_usage(ProviderKind::Deepseek);

        assert_eq!(usage.total_tokens, 150);
        assert_eq!(
            usage.prompt_tokens_details,
            Some(PromptTokensDetails {
                audio_tokens: Some(4),
                cached_tokens: Some(80),
                cache_hit_tokens: Some(75),
                cache_miss_tokens: Some(45),
            })
        );
        assert_eq!(
            usage.completion_tokens_details,
            Some(CompletionTokensDetails {
                accepted_prediction_tokens: Some(2),
                audio_tokens: Some(3),
                reasoning_tokens: Some(20),
                rejected_prediction_tokens: Some(1),
            })
        );
    }

    #[test]
    fn generic_usage_uses_standard_details() {
        let usage: ProviderCompletionUsage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 120,
            "completion_tokens": 30,
            "total_tokens": 150,
            "prompt_tokens_details": {
                "cached_tokens": 80
            },
            "prompt_cache_hit_tokens": 75,
            "prompt_cache_miss_tokens": 45
        }))
        .unwrap();

        let usage = usage.into_completion_usage(ProviderKind::Generic);

        assert_eq!(
            usage.prompt_tokens_details,
            Some(PromptTokensDetails {
                cached_tokens: Some(80),
                ..Default::default()
            })
        );
    }

    fn reduce_openrouter_fixture(fixture: &str) -> AssistantMessage {
        let responses: Vec<CompatibleStreamResponse> = serde_json::from_str(fixture).unwrap();
        let mut completion = CompletionAccumulator::new();
        let mut reasoning_chunks = 0;
        for response in responses {
            let events = ProviderKind::OpenRouter
                .reduce_response("openrouter", response, &mut completion)
                .unwrap();
            reasoning_chunks += events
                .iter()
                .filter(|event| matches!(event, LLMStreamEvent::ReasoningChunk(_)))
                .count();
        }
        assert_eq!(reasoning_chunks, 2);
        AssistantMessage::try_from(completion).unwrap()
    }

    #[test]
    fn real_openrouter_fixtures_keep_ordered_reasoning_details() {
        let cases = [
            (GROK_FIXTURE, 3, "reasoning.summary", "The tool"),
            (KIMI_FIXTURE, 2, "reasoning.text", "Need tool"),
            (GLM_FIXTURE, 2, "reasoning.text", "The user"),
        ];

        for (fixture, expected_details, first_type, expected_reasoning) in cases {
            let message = reduce_openrouter_fixture(fixture);
            assert_eq!(
                message.reasoning_content.as_deref(),
                Some(expected_reasoning)
            );
            assert_eq!(message.tool_calls.len(), 1);
            assert_eq!(
                message.tool_calls[0].arguments.as_deref(),
                Some("{\"city\":\"Singapore\"}")
            );
            let details = message
                .reasoning_continuation
                .as_ref()
                .and_then(|continuation| {
                    continuation.payload_for(OPENROUTER_REASONING_DETAILS_FORMAT)
                })
                .and_then(serde_json::Value::as_array)
                .unwrap();
            assert_eq!(details.len(), expected_details);
            assert_eq!(details[0]["type"], serde_json::json!(first_type));
            assert!(message.usage.is_some());
        }
    }

    #[test]
    fn openrouter_replays_details_and_maps_off_effort_to_none() {
        let continuation = ReasoningContinuation::try_new(
            OPENROUTER_REASONING_DETAILS_FORMAT,
            serde_json::json!([
                {"type": "reasoning.summary", "summary": "first", "index": 0},
                {"type": "reasoning.encrypted", "data": "opaque", "index": 1}
            ]),
        )
        .unwrap();
        let request = ChatCompletionRequest {
            model: "x-ai/grok-4.5".into(),
            messages: vec![Message::Assistant(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "lookup_weather".into(),
                    arguments: Some("{}".into()),
                }],
                reasoning_content: Some("visible".into()),
                reasoning_continuation: Some(continuation),
                ..assistant()
            })],
            reasoning_effort: Some("off".into()),
            ..Default::default()
        };

        let body = ProviderKind::OpenRouter
            .encode_request(request, true)
            .unwrap();

        assert_eq!(body["reasoning"]["effort"], serde_json::json!("none"));
        assert_eq!(
            body["messages"][0]["reasoning_details"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(body["messages"][0].get("reasoning").is_none());
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(
            body["stream_options"]["include_usage"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn openrouter_classifies_malformed_continuation_as_invalid_request() {
        let continuation = ReasoningContinuation::try_new(
            OPENROUTER_REASONING_DETAILS_FORMAT,
            serde_json::json!({"unexpected": "object"}),
        )
        .unwrap();
        let request = ChatCompletionRequest {
            model: "x-ai/grok-4.5".into(),
            messages: vec![Message::Assistant(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "lookup_weather".into(),
                    arguments: Some("{}".into()),
                }],
                reasoning_continuation: Some(continuation),
                ..assistant()
            })],
            ..Default::default()
        };

        let error = ProviderKind::OpenRouter
            .encode_request(request, false)
            .unwrap_err();

        assert!(matches!(
            error,
            StreamError::InvalidRequest(ref message)
                if message == "OpenRouter reasoning continuation payload must be an array"
        ));
    }

    #[test]
    fn openrouter_replays_plain_reasoning_only_for_tool_turns() {
        let request = ChatCompletionRequest {
            model: "moonshotai/kimi-k3".into(),
            messages: vec![
                Message::Assistant(AssistantMessage {
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "lookup_weather".into(),
                        arguments: Some("{}".into()),
                    }],
                    reasoning_content: Some("tool reasoning".into()),
                    ..assistant()
                }),
                Message::Assistant(AssistantMessage {
                    content: "done".into(),
                    reasoning_content: Some("final reasoning".into()),
                    ..assistant()
                }),
            ],
            reasoning_effort: Some("high".into()),
            max_completion_tokens: Some(4096),
            ..Default::default()
        };

        let body = ProviderKind::OpenRouter
            .encode_request(request, false)
            .unwrap();

        assert_eq!(body["messages"][0]["reasoning"], "tool reasoning");
        assert!(body["messages"][1].get("reasoning").is_none());
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["max_completion_tokens"], 4096);
    }

    #[test]
    fn openrouter_keeps_image_input_and_tool_continuation_in_one_request() {
        let continuation = ReasoningContinuation::try_new(
            OPENROUTER_REASONING_DETAILS_FORMAT,
            serde_json::json!([{
                "type": "reasoning.text",
                "text": "inspect image",
                "format": "unknown",
                "index": 0
            }]),
        )
        .unwrap();
        let request = ChatCompletionRequest {
            model: "moonshotai/kimi-k3".into(),
            messages: vec![
                Message::User(coda_core::llm::UserMessage::with_images(
                    "inspect",
                    &["data:image/png;base64,abc123".to_string()],
                )),
                Message::Assistant(AssistantMessage {
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "lookup_weather".into(),
                        arguments: Some("{}".into()),
                    }],
                    reasoning_content: Some("inspect image".into()),
                    reasoning_continuation: Some(continuation),
                    ..assistant()
                }),
            ],
            reasoning_effort: Some("max".into()),
            ..Default::default()
        };

        let body = ProviderKind::OpenRouter
            .encode_request(request, false)
            .unwrap();

        assert_eq!(
            body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,abc123"
        );
        assert_eq!(
            body["messages"][1]["reasoning_details"][0]["text"],
            "inspect image"
        );
    }

    #[test]
    fn openrouter_prefers_reasoning_then_alias_then_visible_details() {
        let responses: Vec<CompatibleStreamResponse> = serde_json::from_value(serde_json::json!([
            {
                "choices": [{"delta": {
                    "reasoning": "primary",
                    "reasoning_content": "alias",
                    "reasoning_details": [{"type": "reasoning.text", "text": "fallback"}]
                }}]
            },
            {
                "choices": [{"delta": {
                    "reasoning_content": " alias",
                    "reasoning_details": [{"type": "reasoning.text", "text": " fallback"}]
                }}]
            },
            {
                "choices": [{"delta": {
                    "reasoning_details": [{"type": "reasoning.summary", "summary": " detail"}]
                }}]
            }
        ]))
        .unwrap();
        let mut completion = CompletionAccumulator::new();
        for response in responses {
            ProviderKind::OpenRouter
                .reduce_response("openrouter", response, &mut completion)
                .unwrap();
        }

        let message = AssistantMessage::try_from(completion).unwrap();
        assert_eq!(
            message.reasoning_content.as_deref(),
            Some("primary alias detail")
        );
    }

    #[test]
    fn openrouter_rejects_stream_error_envelope() {
        let response: CompatibleStreamResponse = serde_json::from_value(serde_json::json!({
            "error": {
                "code": 429,
                "message": "rate limited",
                "metadata": {"error_type": "rate_limit_exceeded"}
            }
        }))
        .unwrap();
        let mut completion = CompletionAccumulator::new();

        let error = match ProviderKind::OpenRouter.reduce_response(
            "openrouter-primary",
            response,
            &mut completion,
        ) {
            Ok(_) => panic!("expected provider error"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            StreamError::Provider(ProviderError {
                ref provider_id,
                status_code: Some(429),
                error_type: Some(ref error_type),
                ref message,
            }) if provider_id == "openrouter-primary"
                && error_type == "rate_limit_exceeded"
                && message == "rate limited"
        ));
    }

    #[test]
    fn openrouter_recovers_structured_non_success_error_from_raw_body() {
        let deserialize_error = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let error = map_request_error(
            ProviderKind::OpenRouter,
            "openrouter-backup",
            OpenAIError::JSONDeserialize(
                deserialize_error,
                serde_json::json!({
                    "error": {
                        "code": 429,
                        "message": "Provider returned error",
                        "metadata": {"error_type": "rate_limit_exceeded"}
                    }
                })
                .to_string(),
            ),
        );

        assert!(matches!(
            error,
            StreamError::Provider(ProviderError {
                ref provider_id,
                status_code: Some(429),
                error_type: Some(ref error_type),
                ref message,
            }) if provider_id == "openrouter-backup"
                && error_type == "rate_limit_exceeded"
                && message == "Provider returned error"
        ));
    }

    #[test]
    fn deepseek_http_api_error_is_a_provider_error() {
        let error = map_request_error(
            ProviderKind::Deepseek,
            "deepseek-primary",
            OpenAIError::ApiError(async_openai::error::ApiErrorResponse {
                status_code: "422".parse().unwrap(),
                api_error: async_openai::error::ApiError {
                    message: "invalid request".into(),
                    r#type: Some("invalid_request_error".into()),
                    param: None,
                    code: Some("invalid_request_error".into()),
                },
            }),
        );

        assert!(matches!(
            error,
            StreamError::Provider(ProviderError {
                ref provider_id,
                status_code: Some(422),
                error_type: Some(ref error_type),
                ref message,
            }) if provider_id == "deepseek-primary"
                && error_type == "invalid_request_error"
                && message == "invalid request"
        ));
    }

    #[test]
    fn compatible_non_success_body_is_still_a_provider_error() {
        let deserialize_error = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let error = map_request_error(
            ProviderKind::Deepseek,
            "deepseek-primary",
            OpenAIError::JSONDeserialize(
                deserialize_error,
                serde_json::json!({
                    "error": {
                        "message": "provider rejected request",
                        "type": "invalid_request_error",
                        "code": "invalid_parameter"
                    }
                })
                .to_string(),
            ),
        );

        assert!(matches!(
            error,
            StreamError::Provider(ProviderError {
                ref provider_id,
                status_code: None,
                error_type: Some(ref error_type),
                ref message,
            }) if provider_id == "deepseek-primary"
                && error_type == "invalid_request_error"
                && message == "provider rejected request"
        ));
    }

    #[test]
    fn malformed_sse_payload_is_an_invalid_response() {
        let deserialize_error = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let error = map_stream_error(
            "deepseek-primary",
            OpenAIError::JSONDeserialize(deserialize_error, "not-json".into()),
        );

        assert!(matches!(
            error,
            StreamError::InvalidResponse(ref message)
                if message.contains("failed to decode provider SSE event")
                    && message.contains("not-json")
        ));
    }

    #[test]
    fn sse_transport_failure_is_a_transport_error() {
        let error = map_stream_error(
            "deepseek-primary",
            OpenAIError::StreamError(Box::new(async_openai::error::StreamError::EventStream(
                "connection reset".into(),
            ))),
        );

        assert!(matches!(
            error,
            StreamError::TransportError(ref message) if message.contains("connection reset")
        ));
    }

    #[test]
    fn accumulator_reassembles_interleaved_parallel_tool_calls() {
        let chunks: Vec<ChatCompletionMessageToolCallChunk> = serde_json::from_value(
            serde_json::json!([
                {"index": 0, "id": "call-0", "type": "function", "function": {"name": "first", "arguments": "{"}},
                {"index": 1, "id": "call-1", "type": "function", "function": {"name": "second", "arguments": "{"}},
                {"index": 0, "function": {"arguments": "}"}},
                {"index": 1, "function": {"arguments": "}"}}
            ]),
        )
        .unwrap();
        let mut completion = CompletionAccumulator::new();
        for chunk in chunks {
            completion.reduce_tool_chunk(chunk);
        }

        let message = AssistantMessage::try_from(completion).unwrap();
        assert_eq!(message.tool_calls.len(), 2);
        assert_eq!(message.tool_calls[0].arguments.as_deref(), Some("{}"));
        assert_eq!(message.tool_calls[1].arguments.as_deref(), Some("{}"));
    }

    #[test]
    fn accumulator_rejects_empty_stream() {
        let error = AssistantMessage::try_from(CompletionAccumulator::new()).unwrap_err();
        assert_eq!(
            error,
            "stream completed without content, reasoning, or tool calls"
        );
    }
}
