use async_openai::Client;
use async_openai::config::OpenAIConfig;
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
    PromptTokensDetails as OpenAIPromptTokensDetails, ReasoningEffort as OpenAIReasoningEffort,
};
use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, CompletionTokensDetails, CompletionUsage, ContentPart,
    LLMProvider, LLMProviderConfig, LLMStreamEvent, Message, PromptTokensDetails, ReasoningEffort,
    StreamError, ToolCall, ToolCallOutcome, ToolDefinition, ToolOutput,
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

/// `CreateChatCompletionStreamResponse` minus the fields we don't consume, plus
/// `reasoning_content` — a non-standard field from OpenAI-compatible reasoning models
/// (e.g. DeepSeek) that async-openai's built-in types drop.
#[derive(Debug, serde::Deserialize)]
struct ReasoningStreamResponse {
    #[serde(default)]
    choices: Vec<ReasoningStreamChoice>,
    usage: Option<ProviderCompletionUsage>,
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
            ProviderKind::Generic => (None, None),
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
struct ReasoningStreamChoice {
    delta: ReasoningStreamDelta,
}

#[derive(Debug, serde::Deserialize)]
struct ReasoningStreamDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
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
}

fn map_effort(effort: ReasoningEffort) -> OpenAIReasoningEffort {
    match effort {
        ReasoningEffort::None => OpenAIReasoningEffort::None,
        ReasoningEffort::Minimal => OpenAIReasoningEffort::Minimal,
        ReasoningEffort::Low => OpenAIReasoningEffort::Low,
        ReasoningEffort::Medium => OpenAIReasoningEffort::Medium,
        ReasoningEffort::High => OpenAIReasoningEffort::High,
        ReasoningEffort::Xhigh => OpenAIReasoningEffort::Xhigh,
    }
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

#[derive(Debug, Clone)]
pub struct OpenAI {
    client: Client<OpenAIConfig>,
    kind: ProviderKind,
    /// Add `stream_options.include_usage` to the request body.
    include_usage: bool,
}

impl LLMProvider for OpenAI {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        let kind = self.kind;
        let reasoning_effort = request.reasoning_effort;
        async_stream::stream! {
            let source_messages = request.messages.clone();
            let messages: Vec<ChatCompletionRequestMessage> = request
                .messages
                .into_iter()
                .map(|x| x.into_openai_type())
                .collect();
            let tools: Vec<ChatCompletionTools> = request
                .tools
                .into_iter()
                .map(|x| x.into_openai_type())
                .collect();
            let mut req = CreateChatCompletionRequestArgs::default();
            req.model(request.model).messages(messages).tools(tools);
            if let Some(temperature) = request.temperature {
                req.temperature(temperature);
            }
            if let Some(max_completion_tokens) = request.max_completion_tokens {
                req.max_completion_tokens(max_completion_tokens);
            }
            // The effort level maps onto the standard `reasoning_effort` field.
            // DeepSeek toggles thinking off via its own `thinking` object instead,
            // so the field is omitted there when reasoning is turned off.
            if let Some(effort) = reasoning_effort {
                let set_effort = match kind {
                    ProviderKind::Generic => true,
                    ProviderKind::Deepseek => effort != ReasoningEffort::None,
                };
                if set_effort {
                    req.reasoning_effort(map_effort(effort));
                }
            }
            let mut req = req.build().unwrap();
            // The `byot` feature disables async-openai's automatic `stream: true`
            // injection, so set it explicitly here.
            req.stream = Some(true);
            if self.include_usage {
                req.stream_options = Some(ChatCompletionStreamOptions {
                    include_usage: Some(true),
                    include_obfuscation: None,
                });
            }
            // Serialize to a JSON body so provider-specific fields (e.g. DeepSeek's
            // `thinking`) can be injected before the request is sent.
            let mut body = serde_json::to_value(&req).unwrap();
            if kind == ProviderKind::Deepseek {
                inject_deepseek_reasoning(&mut body, &source_messages);
                if let Some(effort) = reasoning_effort
                    && let Some(obj) = body.as_object_mut()
                {
                    let state =
                        if effort == ReasoningEffort::None { "disabled" } else { "enabled" };
                    obj.insert("thinking".to_string(), serde_json::json!({ "type": state }));
                }
            }
            let mut stream = match self.client.chat()
                .create_stream_byot::<_, ReasoningStreamResponse>(body)
                .await
            {
                Ok(stream) => stream,
                // A non-2xx response (e.g. a 400 over an invalid tool schema) surfaces
                // here. Some providers return an error body async-openai can't
                // deserialize (e.g. a numeric `code`), so use the Debug form to keep the
                // raw provider message instead of the opaque deserialization error.
                Err(e) => {
                    yield Err(StreamError::StreamingError(format!("{e:?}")));
                    return;
                }
            };
            let mut chat_completion = ReducedChatCompletion::new();
            while let Some(response) = stream.next().await {
                match response {
                    Ok(mut stream_response) => {
                        if let Some(usage) = stream_response.usage {
                            chat_completion.usage = Some(usage.into_completion_usage(kind));
                        }
                        if let Some(choice) = stream_response.choices.pop() {
                            if let Some(reasoning) = &choice.delta.reasoning_content {
                                chat_completion.reduce_reasoning(reasoning);
                                yield Ok(LLMStreamEvent::ReasoningChunk(reasoning.clone()));
                            }
                            if let Some(content) = &choice.delta.content {
                                chat_completion.reduce_content(content);
                                yield Ok(LLMStreamEvent::ContentChunk(content.clone()));
                            }
                            if let Some(calls) = choice.delta.tool_calls {
                                for chunk in calls {
                                    chat_completion.reduce_chunk(chunk);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(StreamError::StreamingError(format!("{}", e)));
                        return;
                    }
                }
            }
            match chat_completion.try_into() {
                Ok(msg) => yield Ok(LLMStreamEvent::Completed(msg)),
                Err(e) => yield Err(StreamError::InvalidResponse(e)),
            }
        }
    }
}

impl OpenAI {
    pub fn new(config: LLMProviderConfig, kind: ProviderKind) -> Self {
        let client_config = OpenAIConfig::new()
            .with_api_base(config.base_url)
            .with_api_key(config.api_key);
        Self {
            client: Client::with_config(client_config),
            kind,
            include_usage: config.include_usage,
        }
    }
}

#[derive(Debug)]
struct ReducedChatCompletion {
    content: String,
    reasoning_content: String,
    chunks: Vec<ChatCompletionMessageToolCallChunk>,
    usage: Option<CompletionUsage>,
}

impl ReducedChatCompletion {
    fn new() -> Self {
        ReducedChatCompletion {
            content: String::new(),
            reasoning_content: String::new(),
            chunks: vec![],
            usage: None,
        }
    }

    fn reduce_content(&mut self, content: &str) {
        self.content += content;
    }

    fn reduce_reasoning(&mut self, content: &str) {
        self.reasoning_content += content;
    }

    fn reduce_chunk(&mut self, chunk: ChatCompletionMessageToolCallChunk) {
        if let Some(mut last) = self.chunks.pop() {
            if chunk.index != last.index {
                self.chunks.push(last);
                self.chunks.push(chunk);
            } else {
                if let Some(id) = chunk.id {
                    last.id = Some(id)
                }
                if let Some(call_stream) = chunk.function {
                    if let Some(call_name) = call_stream.name {
                        if let Some(function) = &mut last.function {
                            if let Some(name) = &mut function.name {
                                *name += &call_name;
                            } else {
                                function.name = Some(call_name);
                            }
                        } else {
                            last.function = Some(FunctionCallStream {
                                name: Some(call_name),
                                arguments: None,
                            })
                        }
                    }

                    if let Some(call_arguments) = call_stream.arguments {
                        if let Some(function) = &mut last.function {
                            if let Some(arguments) = &mut function.arguments {
                                *arguments += &call_arguments;
                            } else {
                                function.arguments = Some(call_arguments);
                            }
                        } else {
                            last.function = Some(FunctionCallStream {
                                name: None,
                                arguments: Some(call_arguments),
                            })
                        }
                    }
                }
                self.chunks.push(last);
            }
        } else {
            self.chunks.push(chunk);
        }
    }
}

impl TryFrom<ReducedChatCompletion> for AssistantMessage {
    type Error = String;

    fn try_from(value: ReducedChatCompletion) -> Result<Self, Self::Error> {
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
        Ok(AssistantMessage {
            content: value.content,
            tool_calls,
            usage: value.usage,
            reasoning_content,
            aborted: false,
            // Timestamps are stamped by the agent runtime, which knows when the
            // request was dispatched and when the stream completed.
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                ..Default::default()
            }),
            Message::Assistant(AssistantMessage {
                content: "done".into(),
                reasoning_content: Some("final reasoning".into()),
                ..Default::default()
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
        let mut completion = ReducedChatCompletion::new();
        completion.reduce_reasoning("first ");
        completion.reduce_reasoning("second");
        completion.reduce_chunk(ChatCompletionMessageToolCallChunk {
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
        let mut completion = ReducedChatCompletion::new();
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
}
