//! Shared fixtures for the driver tests: local tools, a fake LLM provider
//! scripted by system prompt, storage stand-ins, and the `Harness` that drives
//! an `AgentRuntime` and lets a test await its events.

use super::super::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, ModelProfile, RunConfig, Sender, StoredCheckpoint,
    StoredRuntimeSnapshot, SubAgentMode, ToolApprovalMode, ToolCallResolution,
    runtime::{AgentRuntime, AgentRuntimeSnapshot, ResumeTarget, SessionStorage},
};
use coda_core::{
    llm::{
        AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, Message, MessageId,
        ReasoningContinuation, StreamError, ToolCall, ToolMessage,
    },
    tool::{Tool, ToolCallContext, ToolObject, ToolResult, ToolWrapper},
};
use coda_tools::{BuildContext, ReadTodosToolSpec, ToolSpec};
use futures::{Stream, StreamExt, stream};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};
use tokio::{
    sync::{Mutex, Notify},
    time::Duration,
};

/// Base assistant message for tests; callers override the fields they care
/// about with struct-update syntax (`..assistant()`).
pub(super) fn assistant() -> AssistantMessage {
    let now = jiff::Timestamp::now();
    AssistantMessage {
        message_id: MessageId::new(),
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

#[derive(Clone, Default)]
pub(super) struct TestStorage {
    checkpoints: Arc<Mutex<HashMap<String, StoredCheckpoint>>>,
    snapshots: Arc<Mutex<HashMap<String, StoredRuntimeSnapshot>>>,
}

impl TestStorage {
    pub(super) async fn checkpoint(&self, thread_id: &ThreadId) -> Option<StoredCheckpoint> {
        self.checkpoints
            .lock()
            .await
            .get(thread_id.as_ref())
            .cloned()
    }
}

impl SessionStorage for TestStorage {
    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.checkpoints.lock().await.insert(thread_id, checkpoint);
            Ok(())
        })
    }

    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredCheckpoint>, String>> + Send + '_>> {
        let thread_id = thread_id.to_owned();
        Box::pin(async move {
            let checkpoint = self.checkpoints.lock().await.get(&thread_id).cloned();
            Ok(checkpoint)
        })
    }

    fn save_session_snapshot(
        &self,
        session_id: String,
        snapshot: StoredRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.snapshots.lock().await.insert(session_id, snapshot);
            Ok(())
        })
    }

    fn load_session_snapshot(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredRuntimeSnapshot>, String>> + Send + '_>>
    {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            let snapshot = self.snapshots.lock().await.get(&session_id).cloned();
            Ok(snapshot)
        })
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct EchoToolParams {
    text: String,
}

struct EchoTool {
    schema: Schema,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            schema: schemars::schema_for!(EchoToolParams),
        }
    }
}

impl Tool for EchoTool {
    type Parameters = EchoToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo the provided text."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        _ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        async move { Ok(params.text) }
    }
}

pub(super) struct EchoToolSpec;

impl ToolSpec for EchoToolSpec {
    fn name(&self) -> &str {
        "echo"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(EchoTool::new()))
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct SlowToolParams {
    label: String,
}

struct SlowTool {
    schema: Schema,
    gate: Arc<Notify>,
}

impl SlowTool {
    fn new(gate: Arc<Notify>) -> Self {
        Self {
            schema: schemars::schema_for!(SlowToolParams),
            gate,
        }
    }
}

impl Tool for SlowTool {
    type Parameters = SlowToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "slow_tool"
    }

    fn description(&self) -> &str {
        "Waits until the test allows completion."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        _ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let gate = self.gate.clone();
        async move {
            gate.notified().await;
            Ok(params.label)
        }
    }
}

pub(super) struct SlowToolSpec {
    pub(super) gate: Arc<Notify>,
}

impl ToolSpec for SlowToolSpec {
    fn name(&self) -> &str {
        "slow_tool"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(SlowTool::new(self.gate.clone())))
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct CancelAwareToolParams {
    label: String,
}

/// A tool that never completes on its own: it waits for its cancellation
/// token and settles with `ToolError::Aborted` carrying partial output, the
/// way a cancel-aware tool (e.g. shell) tears down and reports back.
struct CancelAwareTool {
    schema: Schema,
}

impl CancelAwareTool {
    fn new() -> Self {
        Self {
            schema: schemars::schema_for!(CancelAwareToolParams),
        }
    }
}

impl Tool for CancelAwareTool {
    type Parameters = CancelAwareToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "cancel_aware"
    }

    fn description(&self) -> &str {
        "Waits for cancellation and reports partial output."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        async move {
            ctx.cancel.cancelled().await;
            Err(ToolError::Aborted(format!(
                "partial output from {}",
                params.label
            )))
        }
    }
}

pub(super) struct CancelAwareToolSpec;

impl ToolSpec for CancelAwareToolSpec {
    fn name(&self) -> &str {
        "cancel_aware"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(CancelAwareTool::new()))
    }
}

#[derive(Clone, Default)]
pub(super) struct TestProvider {
    hold_subagent: Option<Arc<Notify>>,
    hold_generation: Option<Arc<Notify>>,
}

impl TestProvider {
    pub(super) fn with_hold_subagent(hold_subagent: Arc<Notify>) -> Self {
        Self {
            hold_subagent: Some(hold_subagent),
            hold_generation: None,
        }
    }

    pub(super) fn with_hold_generation(hold_generation: Arc<Notify>) -> Self {
        Self {
            hold_generation: Some(hold_generation),
            hold_subagent: None,
        }
    }

    fn completed(
        message: AssistantMessage,
    ) -> Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>> {
        Box::pin(stream::iter(vec![Ok(LLMStreamEvent::Completed(Box::new(
            message,
        )))]))
    }

    fn errored(
        error: StreamError,
    ) -> Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>> {
        Box::pin(stream::iter(vec![Err(error)]))
    }

    fn chunks_then_error(
        error: StreamError,
    ) -> Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>> {
        Box::pin(stream::iter(vec![
            Ok(LLMStreamEvent::ReasoningChunk(
                "uncommitted reasoning".into(),
            )),
            Ok(LLMStreamEvent::ContentChunk("uncommitted content".into())),
            Err(error),
        ]))
    }

    fn chunk_then_wait(
        reasoning: &str,
        chunk: &str,
        gate: Arc<Notify>,
        final_message: AssistantMessage,
    ) -> Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>> {
        Box::pin(
            stream::iter(vec![
                Ok(LLMStreamEvent::ReasoningChunk(reasoning.into())),
                Ok(LLMStreamEvent::ContentChunk(chunk.into())),
            ])
            .chain(stream::once(async move {
                gate.notified().await;
                Ok(LLMStreamEvent::Completed(Box::new(final_message)))
            })),
        )
    }
}

impl LLMProvider for TestProvider {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        let system_prompt = request
            .messages
            .first()
            .and_then(|message| match message {
                Message::System(system) => Some(system.0.as_str()),
                _ => None,
            })
            .unwrap_or_default();

        match system_prompt {
            "main-system" => {
                let has_explore_result = request.messages.iter().any(
                    |message| matches!(message, Message::Tool(tool) if tool.name == "explore"),
                );

                if has_explore_result {
                    Self::completed(AssistantMessage {
                        content: "main done".into(),
                        ..assistant()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_explore".into(),
                            name: "explore".into(),
                            arguments: Some(r#"{"task":"inspect the crate"}"#.into()),
                        }],
                        ..assistant()
                    })
                }
            }
            // Calls `explore` twice in sequence, deliberately reusing one call
            // id. That is legal — a tool call id only has to be unique within
            // its own assistant message — so the two invocations are only
            // distinguishable by which assistant message issued them.
            "twice-main" => {
                let explore_results = request
                    .messages
                    .iter()
                    .filter(
                        |message| matches!(message, Message::Tool(tool) if tool.name == "explore"),
                    )
                    .count();
                if explore_results >= 2 {
                    Self::completed(AssistantMessage {
                        content: "main done".into(),
                        ..assistant()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_explore".into(),
                            name: "explore".into(),
                            arguments: Some(r#"{"task":"inspect the crate"}"#.into()),
                        }],
                        ..assistant()
                    })
                }
            }
            "explore-plain" => Self::completed(AssistantMessage {
                content: "explore done".into(),
                ..assistant()
            }),
            // A middle layer: calls its own sub-agent, then answers.
            "nested-explore" => {
                let has_probe_result = request
                    .messages
                    .iter()
                    .any(|message| matches!(message, Message::Tool(tool) if tool.name == "probe"));
                if has_probe_result {
                    Self::completed(AssistantMessage {
                        content: "explore done".into(),
                        ..assistant()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_probe".into(),
                            name: "probe".into(),
                            arguments: Some(r#"{"task":"probe deeper"}"#.into()),
                        }],
                        ..assistant()
                    })
                }
            }
            "explore-system" => {
                let has_read_todos_result = request.messages.iter().any(
                    |message| matches!(message, Message::Tool(tool) if tool.name == "read_todos"),
                );

                if has_read_todos_result {
                    Self::completed(AssistantMessage {
                        content: "explore done".into(),
                        ..assistant()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_read_todos".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        }],
                        ..assistant()
                    })
                }
            }
            "approval-main" => {
                if tool_message(&request.messages, "call_exec").is_none() {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![
                            ToolCall {
                                id: "call_exec".into(),
                                name: "read_todos".into(),
                                arguments: Some("{}".into()),
                            },
                            ToolCall {
                                id: "call_resolved".into(),
                                name: "read_todos".into(),
                                arguments: Some("{}".into()),
                            },
                            ToolCall {
                                id: "call_rejected".into(),
                                name: "read_todos".into(),
                                arguments: Some("{}".into()),
                            },
                            ToolCall {
                                id: "call_missing".into(),
                                name: "read_todos".into(),
                                arguments: Some("{}".into()),
                            },
                            ToolCall {
                                id: "call_auto".into(),
                                name: "echo".into(),
                                arguments: Some(r#"{"text":"auto"}"#.into()),
                            },
                        ],
                        ..assistant()
                    })
                } else {
                    let ok = matches!(
                        tool_message(&request.messages, "call_exec"),
                        Some(tool)
                            if matches!(tool.outcome, ToolCallOutcome::Approved)
                                && matches!(tool.output, ToolOutput::Ok(ref out) if out == "No todos.")
                    ) && matches!(
                        tool_message(&request.messages, "call_resolved"),
                        Some(tool)
                            if matches!(tool.outcome, ToolCallOutcome::Resolved)
                                && matches!(tool.output, ToolOutput::Ok(ref out) if out == "resolved-by-test")
                    ) && matches!(
                        tool_message(&request.messages, "call_rejected"),
                        Some(tool)
                            if matches!(tool.outcome, ToolCallOutcome::Rejected { reason: Some(ref reason) } if reason == "nope")
                                && matches!(tool.output, ToolOutput::Err(ref out) if out == "nope")
                    ) && matches!(
                        tool_message(&request.messages, "call_missing"),
                        Some(tool)
                            if matches!(tool.outcome, ToolCallOutcome::Rejected { reason: None })
                                && matches!(tool.output, ToolOutput::Err(ref out) if out == "Rejected by user")
                    ) && matches!(
                        tool_message(&request.messages, "call_auto"),
                        Some(tool)
                            if matches!(tool.outcome, ToolCallOutcome::Auto)
                                && matches!(tool.output, ToolOutput::Ok(ref out) if out == "auto")
                    );

                    Self::completed(AssistantMessage {
                        content: if ok {
                            "approval-flow-ok".into()
                        } else {
                            format!("approval-flow-bad: {}", describe_tools(&request.messages))
                        },
                        ..assistant()
                    })
                }
            }
            "interrupt-main" => match last_user(&request.messages) {
                Some("phase1") if tool_message(&request.messages, "call_approve").is_none() => {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_approve".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        }],
                        ..assistant()
                    })
                }
                Some("phase1") if tool_message(&request.messages, "call_approve").is_some() => {
                    Self::completed(AssistantMessage {
                        content: "interrupt-flow-ok".into(),
                        ..assistant()
                    })
                }
                other => Self::completed(AssistantMessage {
                    content: format!("unexpected-user-state: {other:?}"),
                    ..assistant()
                }),
            },
            "abort-main" => Self::completed(AssistantMessage {
                tool_calls: vec![
                    ToolCall {
                        id: "call_slow".into(),
                        name: "slow_tool".into(),
                        arguments: Some(r#"{"label":"slow"}"#.into()),
                    },
                    ToolCall {
                        id: "call_explore".into(),
                        name: "explore".into(),
                        arguments: Some(r#"{"task":"hold"}"#.into()),
                    },
                ],
                ..assistant()
            }),
            "abort-cancel-aware-main" => Self::completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_cancel".into(),
                    name: "cancel_aware".into(),
                    arguments: Some(r#"{"label":"teardown"}"#.into()),
                }],
                ..assistant()
            }),
            "hold-subagent" => {
                let hold_subagent = self
                    .hold_subagent
                    .clone()
                    .expect("hold-subagent prompt requires a notify");
                Box::pin(stream::once(async move {
                    hold_subagent.notified().await;
                    Ok(LLMStreamEvent::Completed(Box::new(AssistantMessage {
                        content: "subagent done".into(),
                        ..assistant()
                    })))
                }))
            }
            "abort-generation-main" => {
                let hold_generation = self
                    .hold_generation
                    .clone()
                    .expect("abort-generation-main prompt requires a notify");
                Self::chunk_then_wait(
                    "partial reasoning",
                    "partial",
                    hold_generation,
                    AssistantMessage {
                        content: "should not complete".into(),
                        ..assistant()
                    },
                )
            }
            "continuation-main" => {
                if tool_message(&request.messages, "call_read_todos").is_some() {
                    let replayed =
                        request.messages.iter().any(|message| {
                            let Message::Assistant(assistant) = message else {
                                return false;
                            };
                            assistant
                                .tool_calls
                                .iter()
                                .any(|call| call.id == "call_read_todos")
                                && assistant.reasoning_continuation.as_ref().and_then(
                                    |continuation| {
                                        continuation.payload_for("openrouter.reasoning_details.v1")
                                    },
                                ) == Some(&serde_json::json!([
                                    {"type": "reasoning.text", "text": "Need todos."},
                                    {"type": "reasoning.encrypted", "data": "opaque"}
                                ]))
                        });
                    Self::completed(AssistantMessage {
                        content: if replayed {
                            "continuation-restored-ok".into()
                        } else {
                            "continuation-missing".into()
                        },
                        ..assistant()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_read_todos".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        }],
                        reasoning_content: Some("Need todos.".into()),
                        reasoning_continuation: Some(
                            ReasoningContinuation::try_new(
                                "openrouter.reasoning_details.v1",
                                serde_json::json!([
                                    {"type": "reasoning.text", "text": "Need todos."},
                                    {"type": "reasoning.encrypted", "data": "opaque"}
                                ]),
                            )
                            .unwrap(),
                        ),
                        ..assistant()
                    })
                }
            }
            "error-main" => Self::errored(StreamError::InvalidResponse("main boom".into())),
            "partial-error-main" => {
                Self::chunks_then_error(StreamError::Provider(coda_core::llm::ProviderError {
                    provider_id: "test-provider".into(),
                    status_code: Some(502),
                    error_type: Some("upstream_error".into()),
                    message: "upstream disconnected".into(),
                }))
            }
            "error-parent-main" => {
                let subagent_failed = matches!(
                    tool_message(&request.messages, "call_explore"),
                    Some(tool)
                        if matches!(tool.output, ToolOutput::Err(ref out) if out.contains("subagent boom"))
                );
                if subagent_failed {
                    Self::completed(AssistantMessage {
                        content: "subagent-error-ok".into(),
                        ..assistant()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_explore".into(),
                            name: "explore".into(),
                            arguments: Some(r#"{"task":"inspect failure"}"#.into()),
                        }],
                        ..assistant()
                    })
                }
            }
            "error-subagent" => Self::errored(StreamError::TransportError("subagent boom".into())),
            other => panic!("unexpected system prompt: {other}"),
        }
    }
}

fn last_user(messages: &[Message]) -> Option<&str> {
    messages.iter().rev().find_map(|message| match message {
        Message::User(user) => user.first_text(),
        _ => None,
    })
}

pub(super) fn tool_message<'a>(messages: &'a [Message], id: &str) -> Option<&'a ToolMessage> {
    messages.iter().find_map(|message| match message {
        Message::Tool(tool) if tool.id == id => Some(tool),
        _ => None,
    })
}

fn describe_tools(messages: &[Message]) -> String {
    let mut tools = messages
        .iter()
        .filter_map(|message| match message {
            Message::Tool(tool) => {
                Some(format!("{}:{:?}:{:?}", tool.id, tool.outcome, tool.output))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    tools.sort();
    tools.join("|")
}

fn user_task(thread_id: &ThreadId, task: &str) -> Envelope {
    Envelope::with_id(|id| Envelope {
        id,
        from: Sender::User,
        to: Receiver {
            name: "coda".into(),
            thread_id: thread_id.clone(),
        },
        reply_to: None,
        body: EnvelopeBody::Task {
            message_id: MessageId::new(),
            task: task.into(),
            images: vec![],
        },
    })
}

/// A `RunConfig` where every agent runs on the fake test model.
pub(super) fn test_config(
    provider: TestProvider,
    approval: ToolApprovalMode,
) -> RunConfig<TestProvider> {
    RunConfig {
        default_model: ModelProfile {
            provider,
            model: "fake".into(),
            label: "fake".into(),
            temperature: None,
            max_completion_tokens: None,
            reasoning_effort: None,
        },
        agent_models: HashMap::new(),
        tool_approval: approval,
        approval_timeout: None,
    }
}

pub(super) struct Harness<S> {
    pub(super) runtime: AgentRuntime,
    events: tokio::sync::broadcast::Receiver<(String, ThreadId, AgentEvent)>,
    pub(super) thread_id: ThreadId,
    pub(super) storage: S,
}

impl<S> Harness<S>
where
    S: SessionStorage + Clone + 'static,
{
    pub(super) async fn start_with_spec(
        storage: S,
        spec: AgentSpec,
        provider: TestProvider,
        approval: ToolApprovalMode,
        initial_task: &str,
    ) -> Self {
        Self::start_with_team(storage, spec, vec![], provider, approval, initial_task).await
    }

    pub(super) async fn start_with_team(
        storage: S,
        root: AgentSpec,
        subagents: Vec<AgentSpec>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        initial_task: &str,
    ) -> Self {
        let agents = AgentTeam::new(root, subagents)
            .expect("valid team")
            .build(".", coda_tools::shared_file_locks());
        Self::start_agents(storage, agents, provider, approval, initial_task).await
    }

    pub(super) async fn start_agents(
        storage: S,
        agents: HashMap<String, Agent>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        initial_task: &str,
    ) -> Self {
        let config = test_config(provider, approval);

        let thread_id = ThreadId::new();
        let mut runtime = AgentRuntime::new(storage.clone(), thread_id.as_ref().to_string());
        runtime
            .bootstrap(agents, None, HashMap::new(), config)
            .await;

        let events = runtime.subscribe();
        let harness = Self {
            runtime,
            events,
            thread_id,
            storage,
        };
        harness.send_task(initial_task).await;
        harness
    }

    pub(super) async fn send_task(&self, task: &str) {
        self.runtime
            .send_message(user_task(&self.thread_id, task))
            .await
            .expect("send task");
    }

    pub(super) async fn send_resume(
        &self,
        agent_name: &str,
        thread_id: &str,
        resolutions: Vec<(String, ToolCallResolution)>,
    ) {
        self.runtime
            .send_message(Envelope::with_id(|id| Envelope {
                id,
                from: Sender::User,
                to: Receiver {
                    name: agent_name.to_string(),
                    thread_id: ThreadId::from(thread_id.to_string()),
                },
                reply_to: None,
                body: EnvelopeBody::Resume(crate::ResumeDecision { resolutions }),
            }))
            .await
            .expect("resume agent");
    }

    /// Restart the harness from storage, injecting resume decisions for
    /// agents that suspended in the previous run (keyed by agent name, carrying
    /// the thread each one is parked on — what `Session::open` derives from the
    /// pending approvals it collected).
    pub(super) async fn restart(
        &self,
        agents: HashMap<String, Agent>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        resume_targets: HashMap<String, (String, ResumeDecision)>,
    ) -> Self {
        self.restart_with_snapshot(agents, provider, approval, resume_targets, true)
            .await
    }

    /// Restart as if the previous process had died without any agent exiting:
    /// the checkpoints are on disk but no runtime snapshot was ever written.
    /// A session a fork just minted starts out in exactly this state.
    pub(super) async fn restart_without_snapshot(
        &self,
        agents: HashMap<String, Agent>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        resume_targets: HashMap<String, (String, ResumeDecision)>,
    ) -> Self {
        self.restart_with_snapshot(agents, provider, approval, resume_targets, false)
            .await
    }

    async fn restart_with_snapshot(
        &self,
        agents: HashMap<String, Agent>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        resume_targets: HashMap<String, (String, ResumeDecision)>,
        keep_snapshot: bool,
    ) -> Self {
        let config = test_config(provider, approval);

        let session_id = self.thread_id.as_ref().to_string();
        let snapshot: Option<AgentRuntimeSnapshot> = if keep_snapshot {
            self.storage
                .load_session_snapshot(&session_id)
                .await
                .unwrap_or_default()
                .map(Into::into)
        } else {
            None
        };
        let resume_targets = resume_targets
            .into_iter()
            .map(|(agent, (thread_id, decision))| {
                (
                    agent,
                    ResumeTarget {
                        thread_id: ThreadId(thread_id),
                        decision,
                    },
                )
            })
            .collect();

        let mut runtime = AgentRuntime::new(self.storage.clone(), session_id.clone());
        let events = runtime.subscribe();
        runtime
            .bootstrap(agents, snapshot, resume_targets, config)
            .await;

        Self {
            runtime,
            events,
            thread_id: ThreadId(session_id),
            storage: self.storage.clone(),
        }
    }

    pub(super) async fn next_event(&mut self) -> (String, ThreadId, AgentEvent) {
        self.events.recv().await.expect("receive event")
    }

    pub(super) async fn shutdown(&self) {
        // Abort first so any in-flight work (e.g. a subagent blocked on a hold
        // gate) is cancelled; then request graceful exit.
        self.runtime.request_abort().await;
        self.runtime.request_exit().await;
        assert!(
            self.runtime
                .wait_for_exit(Some(Duration::from_secs(2)))
                .await,
            "timed out waiting for runtime shutdown"
        );
    }
}

/// Returns `(root, subagents)` for a `coda` root delegating to a single
/// `explore` sub-agent that owns the `read_todos` tool.
pub(super) fn explore_read_todos_specs(main_prompt: &str) -> (AgentSpec, Vec<AgentSpec>) {
    let coda = AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: main_prompt.into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec!["explore".into()],
    };
    let explore = AgentSpec {
        name: "explore".into(),
        description: String::new(),
        system_prompt: "explore-system".into(),
        mode: SubAgentMode::Stateless,
        tools: vec![Box::new(ReadTodosToolSpec)],
        subagents: vec![],
    };
    (coda, vec![explore])
}
