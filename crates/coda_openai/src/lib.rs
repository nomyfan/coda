use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::traits::RequestOptionsBuilder;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCallChunk,
    ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessage,
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestAssistantMessageContentPart,
    ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartText,
    ChatCompletionRequestSystemMessage, ChatCompletionRequestSystemMessageContent,
    ChatCompletionRequestToolMessage, ChatCompletionRequestToolMessageContent,
    ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent, ChatCompletionTool,
    ChatCompletionTools, CreateChatCompletionRequestArgs, FunctionCall, FunctionCallStream,
    FunctionObject,
};
use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, CompletionUsage, LLMProvider, LLMProviderConfig,
    LLMStreamEvent, Message, StreamError, ToolCall, ToolCallOutcome, ToolDefinition, ToolOutput,
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

                ChatCompletionRequestAssistantMessage {
                    content,
                    tool_calls: Some(
                        assistant_message
                            .tool_calls
                            .into_iter()
                            .map(|x| {
                                ChatCompletionMessageToolCalls::Function(
                                    ChatCompletionMessageToolCall {
                                        id: x.id,
                                        function: FunctionCall {
                                            name: x.name,
                                            arguments: if let Some(arguments) = x.arguments {
                                                arguments.clone()
                                            } else {
                                                "".to_string()
                                            },
                                        },
                                    },
                                )
                            })
                            .collect(),
                    ),
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

#[derive(Debug, Clone)]
pub struct OpenAI {
    client: Client<OpenAIConfig>,
    /// Add "stream_options" header with `{"include_usage": true}` to include usage in streaming response.
    stream: bool,
}

impl LLMProvider for OpenAI {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        async_stream::stream! {
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
            let req = req.build().unwrap();
            let mut chat = self.client.chat();
            if self.stream {
                chat = chat
                    .header("stream_options", r#"{"include_usage": true}"#)
                    .unwrap();
            }
            let mut stream = chat.create_stream(req).await.unwrap();
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
    pub fn new(config: LLMProviderConfig) -> Self {
        let client_config = OpenAIConfig::new()
            .with_api_base(config.base_url)
            .with_api_key(config.api_key);
        Self {
            client: Client::with_config(client_config),
            stream: config.stream,
        }
    }
}

#[derive(Debug)]
struct ReducedChatCompletion {
    content: String,
    chunks: Vec<ChatCompletionMessageToolCallChunk>,
    usage: Option<CompletionUsage>,
}

impl ReducedChatCompletion {
    fn new() -> Self {
        ReducedChatCompletion {
            content: String::new(),
            chunks: vec![],
            usage: None,
        }
    }

    fn reduce_content(&mut self, content: &str) {
        self.content += content;
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
        Ok(AssistantMessage {
            content: value.content,
            tool_calls,
            usage: value.usage,
            aborted: false,
        })
    }
}
