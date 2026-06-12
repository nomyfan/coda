use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCallChunk,
    ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessage,
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestAssistantMessageContentPart,
    ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartText,
    ChatCompletionRequestSystemMessage, ChatCompletionRequestSystemMessageContent,
    ChatCompletionRequestToolMessage, ChatCompletionRequestToolMessageContent,
    ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent,
    ChatCompletionStreamOptions, ChatCompletionTool, ChatCompletionTools,
    CompletionUsage as OpenAICompletionUsage, CreateChatCompletionRequestArgs, FunctionCall,
    FunctionCallStream, FunctionObject, ReasoningEffort as OpenAIReasoningEffort,
};
use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, CompletionUsage, LLMProvider, LLMProviderConfig,
    LLMStreamEvent, Message, ReasoningEffort, StreamError, ToolCall, ToolCallOutcome,
    ToolDefinition, ToolOutput,
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
                //
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(user_message.0),
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
                strict: Some(true),
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
    usage: Option<OpenAICompletionUsage>,
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
            let mut stream = self.client.chat()
                .create_stream_byot::<_, ReasoningStreamResponse>(body)
                .await
                .unwrap();
            let mut chat_completion = ReducedChatCompletion::new();
            while let Some(response) = stream.next().await {
                match response {
                    Ok(mut stream_response) => {
                        if let Some(usage) = stream_response.usage {
                            chat_completion.usage = Some(CompletionUsage {
                                prompt_tokens: usage.prompt_tokens,
                                completion_tokens: usage.completion_tokens,
                            });
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
        let reasoning_content = (!tool_calls.is_empty() && !value.reasoning_content.is_empty())
            .then_some(value.reasoning_content);
        Ok(AssistantMessage {
            content: value.content,
            tool_calls,
            usage: value.usage,
            reasoning_content,
            aborted: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn reduced_completion_discards_reasoning_without_tool_calls() {
        let mut completion = ReducedChatCompletion::new();
        completion.reduce_reasoning("final reasoning");

        let message = AssistantMessage::try_from(completion).unwrap();

        assert!(message.reasoning_content.is_none());
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
}
