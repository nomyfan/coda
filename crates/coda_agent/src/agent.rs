use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, Message, StreamError,
    ToolCall, ToolCallOutcome, ToolMessage, ToolOutput, UserMessage,
};
use coda_core::tool::ToolManager;
use futures::{Stream, StreamExt};
use tracing::{debug, error};

use crate::tools::{
    GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, ReadTodosTool, ShellTool, WriteFileTool,
    WriteTodosTool,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub title: String,
    pub done: bool,
}

pub enum ToolApprovalMode {
    Auto,
    Manual,
    RequireWhen(Arc<dyn Fn(&ToolCall) -> bool + Send + Sync>),
}

/// Caller's resolution for a single suspended tool call.
pub enum ToolCallResolution {
    /// The agent should execute this call.
    Execute,
    /// The caller already handled it; use this result directly.
    Resolved(ToolOutput),
    /// The caller rejected execution.
    Rejected { reason: Option<String> },
}

/// Caller's response to all suspended tool calls, replacing `ApprovalDecision`.
pub struct ResumeDecision {
    pub resolutions: Vec<(String, ToolCallResolution)>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentCheckpoint {
    pub thread_id: String,
    pub messages: Vec<Message>,
    /// Tool calls that require human approval before execution.
    pub pending_calls: Vec<ToolCall>,
    /// Tool calls that can be executed automatically without approval.
    pub auto_calls: Vec<ToolCall>,
    pub todos: Vec<TodoItem>,
}

pub struct AgentState {
    messages: Vec<Message>,
    pub todos: Vec<TodoItem>,
}

/// Events produced by `Agent::run` and `Agent::resume`.
pub enum AgentEvent {
    LLMStart(ChatCompletionRequest),
    LLMContentChunk(String),
    LLMEnd(AssistantMessage),
    ToolCallStart(ToolCall),
    ToolCallEnd(ToolMessage),
    /// Emitted when tool calls require human approval. The stream terminates after this event.
    /// Call `Agent::resume` with the checkpoint and a `ResumeDecision` to continue.
    Suspended(AgentCheckpoint),
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
    pub thread_id: String,
    pub tool_approval: ToolApprovalMode,
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
        debug!("Adding message: {:?}", message);
        self.state.lock().await.messages.push(message);
    }

    pub async fn messages(&self) -> Vec<Message> {
        self.state.lock().await.messages.clone()
    }

    /// Append previously saved (non-system) messages and restore todos.
    /// The system message must already have been added before calling this.
    pub async fn restore_history(&self, messages: Vec<Message>, todos: Vec<TodoItem>) {
        let mut state = self.state.lock().await;
        state.messages.extend(messages);
        state.todos = todos;
    }

    /// Core run loop shared by `run` and `resume`. Drives the LLM ↔ tool execution cycle
    /// until the model stops requesting tools or a suspension point is reached.
    fn run_loop<'a>(
        &'a self,
        config: &'a RunConfig,
    ) -> impl Stream<Item = Result<AgentEvent, StreamError>> + 'a {
        async_stream::try_stream! {
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
                    let (pending_calls, auto_calls): (Vec<ToolCall>, Vec<ToolCall>) = match &config.tool_approval {
                        ToolApprovalMode::Auto => (vec![], assistant_message.tool_calls.clone()),
                        ToolApprovalMode::Manual => (assistant_message.tool_calls.clone(), vec![]),
                        ToolApprovalMode::RequireWhen(predicate) => {
                            assistant_message.tool_calls.clone().into_iter().partition(|c| predicate(c))
                        }
                    };

                    if !pending_calls.is_empty() {
                        let state = self.state.lock().await;
                        let checkpoint = AgentCheckpoint {
                            thread_id: config.thread_id.clone(),
                            messages: state.messages.clone(),
                            pending_calls,
                            auto_calls,
                            todos: state.todos.clone(),
                        };
                        drop(state);
                        yield AgentEvent::Suspended(checkpoint);
                        break;
                    }

                    for call in &assistant_message.tool_calls {
                        yield AgentEvent::ToolCallStart(call.clone());
                    }

                    let futures: Vec<_> = assistant_message
                        .tool_calls
                        .into_iter()
                        .map(|call| {
                            let tool = self.tools.get(&call.name);
                            async move {
                                let output = match tool {
                                    Some(t) => match t
                                        .execute(call.arguments.unwrap_or_default())
                                        .await
                                    {
                                        Ok(s) => ToolOutput::Ok(s),
                                        Err(e) => ToolOutput::Err(e.to_string()),
                                    },
                                    None => ToolOutput::Err(format!(
                                        "Tool '{}' not found",
                                        call.name
                                    )),
                                };
                                ToolMessage {
                                    id: call.id,
                                    name: call.name,
                                    output,
                                    outcome: ToolCallOutcome::Auto,
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

    /// Continue the run loop from the current state, without requiring a new user message.
    /// Use this after manually injecting a `ToolMessage` into the conversation (e.g. for
    /// interactive tools handled entirely on the CLI side).
    pub fn continue_run(
        &self,
        config: RunConfig,
    ) -> impl Stream<Item = Result<AgentEvent, StreamError>> + '_ {
        async_stream::try_stream! {
            let mut inner = std::pin::pin!(self.run_loop(&config));
            while let Some(event) = inner.next().await {
                yield event?;
            }
        }
    }

    pub fn run(
        &self,
        user_message: UserMessage,
        config: RunConfig,
    ) -> impl Stream<Item = Result<AgentEvent, StreamError>> + '_ {
        async_stream::try_stream! {
            self.add_message(Message::User(user_message)).await;
            let mut inner = std::pin::pin!(self.run_loop(&config));
            while let Some(event) = inner.next().await {
                yield event?;
            }
        }
    }

    /// Resume from a checkpoint after the caller has resolved all suspended tool calls.
    ///
    /// Each pending call is matched against its `ToolCallResolution`:
    /// - `Execute` → queued for agent execution (outcome: `Approved`)
    /// - `Resolved(output)` → injected directly (outcome: `Resolved`)
    /// - `Rejected { reason }` → error injected (outcome: `Rejected`)
    ///
    /// Auto calls from the checkpoint are always executed (outcome: `Auto`).
    pub fn resume(
        &self,
        checkpoint: AgentCheckpoint,
        decision: ResumeDecision,
        config: RunConfig,
    ) -> impl Stream<Item = Result<AgentEvent, StreamError>> + '_ {
        async_stream::try_stream! {
            let mut resolution_map: HashMap<String, ToolCallResolution> =
                decision.resolutions.into_iter().collect();

            // Every pending call must have a resolution. If not, re-suspend
            // with the original checkpoint so the caller can fix the issue.
            if checkpoint.pending_calls.iter().any(|c| !resolution_map.contains_key(&c.id)) {
                yield AgentEvent::Suspended(checkpoint);
                return;
            }

            // Restore conversation state from checkpoint.
            {
                let mut state = self.state.lock().await;
                state.messages = checkpoint.messages;
                state.todos = checkpoint.todos;
            }

            // Process each pending call according to its resolution.
            let mut calls_to_execute: Vec<ToolCall> = Vec::new();
            for call in checkpoint.pending_calls {
                match resolution_map.remove(&call.id) {
                    Some(ToolCallResolution::Resolved(output)) => {
                        let tool_message = ToolMessage {
                            id: call.id,
                            name: call.name,
                            output,
                            outcome: ToolCallOutcome::Resolved,
                        };
                        yield AgentEvent::ToolCallEnd(tool_message.clone());
                        self.add_message(Message::Tool(tool_message)).await;
                    }
                    Some(ToolCallResolution::Rejected { reason }) => {
                        let err_msg = match &reason {
                            Some(r) => format!("tool call rejected by user, reason: {r}"),
                            None => "tool call rejected by user".to_string(),
                        };
                        let tool_message = ToolMessage {
                            id: call.id,
                            name: call.name,
                            output: ToolOutput::Err(err_msg),
                            outcome: ToolCallOutcome::Rejected { reason },
                        };
                        yield AgentEvent::ToolCallEnd(tool_message.clone());
                        self.add_message(Message::Tool(tool_message)).await;
                    }
                    Some(ToolCallResolution::Execute) => {
                        calls_to_execute.push(call);
                    }
                    None => {
                        error!("every pending call should have a resolution, but call ID {} is missing", call.id);
                    },
                }
            }

            // Track how many are approved (from pending Execute) before appending auto_calls.
            let approved_count = calls_to_execute.len();
            calls_to_execute.extend(checkpoint.auto_calls);

            for call in &calls_to_execute {
                yield AgentEvent::ToolCallStart(call.clone());
            }

            let futures: Vec<_> = calls_to_execute
                .into_iter()
                .enumerate()
                .map(|(i, call)| {
                    let tool = self.tools.get(&call.name);
                    let is_approved = i < approved_count;
                    async move {
                        let output = match tool {
                            Some(t) => match t
                                .execute(call.arguments.unwrap_or_default())
                                .await
                            {
                                Ok(s) => ToolOutput::Ok(s),
                                Err(e) => ToolOutput::Err(e.to_string()),
                            },
                            None => ToolOutput::Err(format!(
                                "Tool '{}' not found",
                                call.name
                            )),
                        };
                        ToolMessage {
                            id: call.id,
                            name: call.name,
                            output,
                            outcome: if is_approved {
                                ToolCallOutcome::Approved
                            } else {
                                ToolCallOutcome::Auto
                            },
                        }
                    }
                })
                .collect();

            for tool_message in futures::future::join_all(futures).await {
                yield AgentEvent::ToolCallEnd(tool_message.clone());
                self.add_message(Message::Tool(tool_message)).await;
            }

            // Continue the run loop with the same config.
            let mut inner = std::pin::pin!(self.run_loop(&config));
            while let Some(event) = inner.next().await {
                yield event?;
            }
        }
    }
}
