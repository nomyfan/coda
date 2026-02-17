use crate::core::llm::{AssistantMessage, CompletionUsage, Message, ToolCall, ToolDescriptor};
use crate::core::llm::{ChatCompletionRequest, LLM};
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCallChunk,
    ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessage,
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessage, ChatCompletionRequestSystemMessageContent,
    ChatCompletionRequestToolMessage, ChatCompletionRequestToolMessageContent,
    ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent, ChatCompletionTool,
    ChatCompletionTools, CreateChatCompletionRequestArgs, FunctionCall, FunctionCallStream,
    FunctionObject,
};
use futures::StreamExt;

impl From<Message> for ChatCompletionRequestMessage {
    fn from(value: Message) -> Self {
        match value {
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
                //
                ChatCompletionRequestAssistantMessage {
                    content: Some(ChatCompletionRequestAssistantMessageContent::Text(
                        assistant_message.content,
                    )),
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
                //
                ChatCompletionRequestToolMessage {
                    tool_call_id: tool_message.id,
                    content: ChatCompletionRequestToolMessageContent::Text(
                        tool_message.result.to_string(),
                    ),
                }
                .into()
            }
        }
    }
}

impl From<ToolDescriptor> for ChatCompletionTools {
    fn from(value: ToolDescriptor) -> Self {
        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: value.name,
                description: Some(value.description),
                parameters: Some(value.parameter_schema),
                strict: Some(true),
            },
        })
    }
}

pub(crate) struct OpenAI {
    client: Client<OpenAIConfig>,
}

impl OpenAI {
    pub(crate) fn new(llm: LLM) -> Self {
        let mut config = OpenAIConfig::new()
            .with_api_base(llm.base_url)
            .with_api_key(llm.api_key);
        if llm.stream {
            config = config
                .with_header("stream_options", r#"{"include_usage": true}"#)
                .unwrap();
        }
        Self {
            client: Client::with_config(config),
        }
    }

    pub(crate) async fn stream(
        &self,
        request: ChatCompletionRequest,
        mut on_content: impl AsyncFnMut(String),
        // TODO: better error type
    ) -> Result<AssistantMessage, String> {
        let messages: Vec<ChatCompletionRequestMessage> =
            request.messages.into_iter().map(|x| x.into()).collect();
        let tools: Vec<ChatCompletionTools> = request.tools.into_iter().map(|x| x.into()).collect();
        let mut req = CreateChatCompletionRequestArgs::default();
        req.model(request.model).messages(messages).tools(tools);
        if let Some(temperature) = request.temperature {
            req.temperature(temperature);
        }
        if let Some(max_completion_tokens) = request.max_completion_tokens {
            req.max_completion_tokens(max_completion_tokens);
        }
        let req = req.build().unwrap();
        let mut stream = self.client.chat().create_stream(req).await.unwrap();
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
                            on_content(content.clone()).await;
                        }
                        if let Some(calls) = choice.delta.tool_calls {
                            for chunk in calls {
                                chat_completion.reduce_chunk(chunk);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error: {:?}", e);
                }
            }
        }
        Ok(chat_completion.try_into().expect("TODO:"))
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
    type Error = String; // TODO: define a proper error type

    fn try_from(value: ReducedChatCompletion) -> Result<Self, Self::Error> {
        Ok(AssistantMessage {
            content: value.content,
            tool_calls: value
                .chunks
                .into_iter()
                .map(|x| {
                    let id = x.id.expect("TODO: expect an tool call id");
                    let function = x.function.expect("TODO: expect a function call");
                    let name = function.name.expect("TODO: expect a function call name");
                    let arguments = function.arguments;
                    ToolCall {
                        id,
                        name,
                        arguments,
                    }
                })
                .collect(),
            usage: value.usage,
        })
    }
}
