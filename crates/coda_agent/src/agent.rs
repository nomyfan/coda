use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, Message, StreamError,
    ToolCall, ToolCallOutcome, ToolMessage, ToolOutput, UserMessage,
};
use coda_core::tool::ToolManager;
use futures::{FutureExt, Stream, StreamExt};
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

/// Identifies what was interrupted by an abort.
pub enum AbortedTarget {
    /// LLM generation was interrupted.
    Generation,
    /// Tool execution was interrupted; carries the IDs of unfinished tool calls.
    ToolCalls(Vec<String>),
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
    /// Emitted when the run is aborted by the user. The stream terminates after this event.
    Aborted(AbortedTarget),
}

pub struct Agent<P: LLMProvider> {
    pub provider: P,
    pub state: Arc<Mutex<AgentState>>,
    pub tools: ToolManager,
    cancel_token: std::sync::Mutex<CancellationToken>,
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
            cancel_token: std::sync::Mutex::new(CancellationToken::new()),
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

    pub async fn add_messages(&self, messages: Vec<Message>) {
        debug!("Adding messages: {:?}", messages);
        self.state.lock().await.messages.extend(messages);
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

    /// Request cancellation of the current run. The agent will yield an
    /// `Aborted` event and stop. Safe to call from any thread.
    pub fn abort(&self) {
        self.cancel_token.lock().unwrap().cancel();
    }

    /// Execute tool calls concurrently with cancellation support.
    ///
    /// Yields `ToolCallStart` for each call, then executes them via `FuturesUnordered`.
    /// Completed results are yielded as `ToolCallEnd` and written to history immediately.
    /// On cancellation, already-completed results are preserved; unfinished calls get
    /// `ToolCallOutcome::Aborted` and the stream ends with `AgentEvent::Aborted`.
    fn execute_tool_calls<'a>(
        &'a self,
        calls: Vec<(ToolCall, ToolCallOutcome)>,
        cancel: &'a CancellationToken,
    ) -> impl Stream<Item = Result<AgentEvent, StreamError>> + 'a {
        async_stream::try_stream! {
            for (call, _) in &calls {
                yield AgentEvent::ToolCallStart(call.clone());
            }

            let tool_futs = futures::stream::FuturesUnordered::new();
            let mut pending_ids: HashMap<String, String> = HashMap::new();
            for (call, outcome) in calls {
                pending_ids.insert(call.id.clone(), call.name.clone());
                let tool = self.tools.get(&call.name);
                tool_futs.push(async move {
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
                        outcome,
                    }
                });
            }

            let mut aborted = false;
            let mut tool_futs = std::pin::pin!(tool_futs);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        aborted = true;
                        break;
                    }
                    result = tool_futs.next() => {
                        match result {
                            Some(tool_message) => {
                                pending_ids.remove(&tool_message.id);
                                self.add_message(Message::Tool(tool_message.clone())).await;
                                yield AgentEvent::ToolCallEnd(tool_message);
                            }
                            None => break,
                        }
                    }
                }
            }

            if aborted {
                // Drain any results that completed concurrently.
                while let Some(tool_message) = tool_futs.next().now_or_never().flatten() {
                    pending_ids.remove(&tool_message.id);
                    self.add_message(Message::Tool(tool_message.clone())).await;
                    yield AgentEvent::ToolCallEnd(tool_message);
                }

                // Write aborted results for unfinished tool calls.
                let aborted_ids: Vec<String> = pending_ids.keys().cloned().collect();
                let aborted_messages: Vec<Message> = pending_ids
                    .into_iter()
                    .map(|(id, name)| {
                        Message::Tool(ToolMessage {
                            id,
                            name,
                            output: ToolOutput::Err("Aborted by user".to_string()),
                            outcome: ToolCallOutcome::Aborted,
                        })
                    })
                    .collect();
                self.add_messages(aborted_messages).await;

                yield AgentEvent::Aborted(AbortedTarget::ToolCalls(aborted_ids));
            }
        }
    }

    /// Core run loop shared by `run` and `resume`. Drives the LLM ↔ tool execution cycle
    /// until the model stops requesting tools or a suspension point is reached.
    fn run_loop<'a>(
        &'a self,
        config: &'a RunConfig,
    ) -> impl Stream<Item = Result<AgentEvent, StreamError>> + 'a {
        async_stream::try_stream! {
            let cancel = self.cancel_token.lock().unwrap().clone();

            loop {
                let request = ChatCompletionRequest {
                    model: config.model.clone(),
                    messages: self.messages().await,
                    tools: self.tools.descriptors(),
                    max_completion_tokens: config.max_completion_tokens,
                    temperature: config.temperature,
                };

                yield AgentEvent::LLMStart(request.clone());

                // --- LLM streaming phase with abort support ---
                let mut llm_stream = std::pin::pin!(self.provider.stream(request));
                let mut assistant_message = None;
                let mut partial_content = String::new();
                let mut aborted_in_llm = false;
                let mut llm_error: Option<StreamError> = None;

                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            aborted_in_llm = true;
                            break;
                        }
                        event = llm_stream.next() => {
                            match event {
                                Some(Ok(LLMStreamEvent::ContentChunk(s))) => {
                                    partial_content.push_str(&s);
                                    yield AgentEvent::LLMContentChunk(s);
                                }
                                Some(Ok(LLMStreamEvent::Completed(msg))) => {
                                    assistant_message = Some(msg);
                                    break;
                                }
                                Some(Err(e)) => {
                                    llm_error = Some(e);
                                    break;
                                }
                                None => {
                                    break;
                                }
                            }
                        }
                    }
                }

                if let Some(e) = llm_error {
                    Err(e)?;
                }

                if aborted_in_llm {
                    // Only write a partial AssistantMessage if there was actual content.
                    if !partial_content.is_empty() {
                        let partial_msg = AssistantMessage {
                            content: partial_content,
                            aborted: true,
                            ..Default::default()
                        };
                        self.add_message(Message::Assistant(partial_msg)).await;
                    }
                    yield AgentEvent::Aborted(AbortedTarget::Generation);
                    break;
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

                    let calls = assistant_message.tool_calls.into_iter()
                        .map(|c| (c, ToolCallOutcome::Auto))
                        .collect();
                    let mut exec = std::pin::pin!(self.execute_tool_calls(calls, &cancel));
                    let mut was_aborted = false;
                    while let Some(event) = exec.next().await {
                        let event = event?;
                        if matches!(&event, AgentEvent::Aborted(_)) {
                            was_aborted = true;
                        }
                        yield event;
                    }
                    if was_aborted {
                        break;
                    }
                }

                if stop {
                    break;
                }
            }
        }
    }

    /// Reset the cancellation token so a previous abort does not affect the new run.
    fn reset_cancel_token(&self) {
        let mut token = self.cancel_token.lock().unwrap();
        if token.is_cancelled() {
            *token = CancellationToken::new();
        }
    }

    /// Continue the run loop from the current state, without requiring a new user message.
    /// Use this after manually injecting a `ToolMessage` into the conversation (e.g. for
    /// interactive tools handled entirely on the CLI side).
    pub fn continue_run(
        &self,
        config: RunConfig,
    ) -> impl Stream<Item = Result<AgentEvent, StreamError>> + '_ {
        self.reset_cancel_token();
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
        self.reset_cancel_token();
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
        self.reset_cancel_token();
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
            let mut calls_to_execute: Vec<(ToolCall, ToolCallOutcome)> = Vec::new();
            for call in checkpoint.pending_calls {
                match resolution_map.remove(&call.id) {
                    Some(ToolCallResolution::Resolved(output)) => {
                        let tool_message = ToolMessage {
                            id: call.id,
                            name: call.name,
                            output,
                            outcome: ToolCallOutcome::Resolved,
                        };
                        self.add_message(Message::Tool(tool_message.clone())).await;
                        yield AgentEvent::ToolCallEnd(tool_message);
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
                        self.add_message(Message::Tool(tool_message.clone())).await;
                        yield AgentEvent::ToolCallEnd(tool_message);
                    }
                    Some(ToolCallResolution::Execute) => {
                        calls_to_execute.push((call, ToolCallOutcome::Approved));
                    }
                    None => {
                        error!("every pending call should have a resolution, but call ID {} is missing", call.id);
                    },
                }
            }

            // Auto calls from the checkpoint are always executed.
            calls_to_execute.extend(
                checkpoint.auto_calls.into_iter()
                    .map(|c| (c, ToolCallOutcome::Auto))
            );

            let cancel = self.cancel_token.lock().unwrap().clone();
            let mut exec = std::pin::pin!(self.execute_tool_calls(calls_to_execute, &cancel));
            while let Some(event) = exec.next().await {
                let event = event?;
                if matches!(&event, AgentEvent::Aborted(_)) {
                    yield event;
                    return;
                }
                yield event;
            }

            // Continue the run loop with the same config.
            let mut inner = std::pin::pin!(self.run_loop(&config));
            while let Some(event) = inner.next().await {
                yield event?;
            }
        }
    }
}
