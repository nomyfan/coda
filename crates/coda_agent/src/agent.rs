use std::sync::Arc;
use tokio::sync::Mutex;

use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, Message, StreamError,
    ToolCall, ToolMessage, UserMessage,
};
use coda_core::tool::ToolManager;
use futures::{Stream, StreamExt};

use crate::tools::{
    GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, ReadTodosTool, ShellTool, WriteFileTool,
    WriteTodosTool,
};

#[derive(Debug, Clone)]
pub struct TodoItem {
    pub title: String,
    pub done: bool,
}

pub struct AgentState {
    messages: Vec<Message>,
    pub todos: Vec<TodoItem>,
}

/// Events produced by `Agent::run`.
pub enum AgentEvent {
    LLMStart(ChatCompletionRequest),
    LLMContentChunk(String),
    LLMEnd(AssistantMessage),
    ToolCallStart(ToolCall),
    ToolCallEnd(ToolMessage),
}

pub struct Agent<P: LLMProvider> {
    pub provider: P,
    pub state: Arc<Mutex<AgentState>>,
    pub tools: ToolManager,
}

pub struct RunConfig {
    pub model: String,
    pub temperature: Option<f32>,
    pub max_completion_tokens: Option<u32>,
}

impl<P: LLMProvider> Agent<P> {
    pub fn new(provider: P) -> Self {
        let state = Arc::new(Mutex::new(AgentState {
            messages: vec![],
            todos: vec![],
        }));

        Agent {
            provider,
            state,
            tools: ToolManager::new(),
        }
    }

    pub fn new_with_default_tools(provider: P, workspace_dir: impl Into<String>) -> Self {
        let mut agent = Self::new(provider);
        let cwd = workspace_dir.into();
        let state = agent.state.clone();
        agent.tools.register(ShellTool::new());
        agent.tools.register(ReadFileTool::new());
        agent.tools.register(WriteFileTool::new());
        agent.tools.register(ListDirectoryTool::new());
        agent.tools.register(GrepTool::new(cwd.clone()));
        agent.tools.register(GlobTool::new(cwd));
        agent.tools.register(ReadTodosTool::new(state.clone()));
        agent.tools.register(WriteTodosTool::new(state));
        agent
    }

    pub fn state(&self) -> Arc<Mutex<AgentState>> {
        self.state.clone()
    }

    pub async fn add_message(&self, message: Message) {
        self.state.lock().await.messages.push(message);
    }

    pub async fn messages(&self) -> Vec<Message> {
        self.state.lock().await.messages.clone()
    }

    pub fn run(
        &self,
        user_message: UserMessage,
        config: RunConfig,
    ) -> impl Stream<Item = Result<AgentEvent, StreamError>> + '_ {
        async_stream::try_stream! {
            self.add_message(Message::User(user_message)).await;
            loop {
                let request = ChatCompletionRequest {
                    model: config.model.clone(),
                    messages: self.messages().await,
                    tools: self.tools.descriptors(),
                    max_completion_tokens: config.max_completion_tokens,
                    temperature: config.temperature,
                };

                yield AgentEvent::LLMStart(request.clone());

                let mut llm_stream = std::pin::pin!(self.provider.stream(request));
                let mut assistant_message = None;
                while let Some(event) = llm_stream.next().await {
                    match event? {
                        LLMStreamEvent::ContentChunk(s) => {
                            yield AgentEvent::LLMContentChunk(s);
                        }
                        LLMStreamEvent::Completed(msg) => {
                            assistant_message = Some(msg);
                        }
                    }
                }

                let assistant_message = assistant_message.ok_or_else(|| {
                    StreamError::InvalidResponse("LLM stream ended without Completed event".into())
                })?;

                yield AgentEvent::LLMEnd(assistant_message.clone());

                let stop = assistant_message.tool_calls.is_empty();
                self.add_message(Message::Assistant(assistant_message.clone())).await;

                if !assistant_message.tool_calls.is_empty() {
                    // Yield all ToolCallStart events first
                    for call in &assistant_message.tool_calls {
                        yield AgentEvent::ToolCallStart(call.clone());
                    }

                    // Execute all tools in parallel
                    let futures: Vec<_> = assistant_message
                        .tool_calls
                        .into_iter()
                        .map(|call| {
                            let tool = self.tools.get(&call.name);
                            async move {
                                let result = match tool {
                                    Some(t) => t
                                        .execute(call.arguments.unwrap_or_default())
                                        .await
                                        .unwrap_or_else(|e| format!("Error: {e}")),
                                    None => format!("Error: Tool '{}' not found", call.name),
                                };
                                ToolMessage {
                                    id: call.id,
                                    name: call.name,
                                    result,
                                }
                            }
                        })
                        .collect();

                    for tool_message in futures::future::join_all(futures).await {
                        yield AgentEvent::ToolCallEnd(tool_message.clone());
                        self.add_message(Message::Tool(tool_message)).await;
                    }
                }

                if stop {
                    break;
                }
            }
        }
    }
}
