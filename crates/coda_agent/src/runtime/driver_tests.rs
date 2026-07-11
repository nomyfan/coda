use super::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, ModelProfile, RunConfig, Sender, StoredCheckpoint,
    StoredRuntimeSnapshot, SubAgentMode, ToolApprovalMode, ToolCallResolution,
    runtime::{AgentRuntime, AgentRuntimeSnapshot, MemoryStorage, SessionStorage},
};
use coda_core::{
    llm::{
        AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, Message, StreamError,
        ToolCall, ToolMessage,
    },
    tool::{Tool, ToolCallContext, ToolObject, ToolResult, ToolWrapper},
};
use coda_tools::{BuildContext, ReadTodosToolSpec, ToolSpec};
use futures::{Stream, StreamExt, stream};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
};
use tokio::{
    sync::{Mutex, Notify},
    task::yield_now,
    time::{Duration, timeout},
};

/// Base assistant message for tests; callers override the fields they care
/// about with struct-update syntax (`..assistant()`).
fn assistant() -> AssistantMessage {
    let now = jiff::Timestamp::now();
    AssistantMessage {
        content: String::new(),
        tool_calls: vec![],
        usage: None,
        reasoning_content: None,
        reasoning_ended_at: None,
        aborted: false,
        started_at: now,
        ended_at: now,
    }
}

#[derive(Clone, Default)]
struct TestStorage {
    checkpoints: Arc<Mutex<HashMap<String, StoredCheckpoint>>>,
    snapshots: Arc<Mutex<HashMap<String, StoredRuntimeSnapshot>>>,
}

impl TestStorage {
    async fn checkpoint(&self, thread_id: &ThreadId) -> Option<StoredCheckpoint> {
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
struct EchoToolParams {
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

struct EchoToolSpec;

impl ToolSpec for EchoToolSpec {
    fn name(&self) -> &str {
        "echo"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(EchoTool::new()))
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SlowToolParams {
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

struct SlowToolSpec {
    gate: Arc<Notify>,
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
struct CancelAwareToolParams {
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

struct CancelAwareToolSpec;

impl ToolSpec for CancelAwareToolSpec {
    fn name(&self) -> &str {
        "cancel_aware"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(CancelAwareTool::new()))
    }
}

#[derive(Clone, Default)]
struct TestProvider {
    hold_subagent: Option<Arc<Notify>>,
    hold_generation: Option<Arc<Notify>>,
}

impl TestProvider {
    fn with_hold_subagent(hold_subagent: Arc<Notify>) -> Self {
        Self {
            hold_subagent: Some(hold_subagent),
            hold_generation: None,
        }
    }

    fn with_hold_generation(hold_generation: Arc<Notify>) -> Self {
        Self {
            hold_generation: Some(hold_generation),
            hold_subagent: None,
        }
    }

    fn completed(
        message: AssistantMessage,
    ) -> Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>> {
        Box::pin(stream::iter(vec![Ok(LLMStreamEvent::Completed(message))]))
    }

    fn errored(
        error: StreamError,
    ) -> Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>> {
        Box::pin(stream::iter(vec![Err(error)]))
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
                Ok(LLMStreamEvent::Completed(final_message))
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
                    Ok(LLMStreamEvent::Completed(AssistantMessage {
                        content: "subagent done".into(),
                        ..assistant()
                    }))
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
            // Echoes the first line of every user message it sees, in order,
            // so tests can assert what reached the model and in what order.
            "notice-main" => Self::completed(AssistantMessage {
                content: format!(
                    "seen:{}",
                    request
                        .messages
                        .iter()
                        .filter_map(|message| match message {
                            Message::User(user) => user.first_text().map(|text| text
                                .lines()
                                .next()
                                .unwrap_or("")
                                .to_string()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("|")
                ),
                ..assistant()
            }),
            "error-main" => Self::errored(StreamError::InvalidResponse("main boom".into())),
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
            "error-subagent" => Self::errored(StreamError::StreamingError("subagent boom".into())),
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

fn tool_message<'a>(messages: &'a [Message], id: &str) -> Option<&'a ToolMessage> {
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
            task: task.into(),
            images: vec![],
        },
    })
}

/// A `RunConfig` where every agent runs on the fake test model.
fn test_config(provider: TestProvider, approval: ToolApprovalMode) -> RunConfig<TestProvider> {
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

struct Harness<S> {
    runtime: AgentRuntime,
    events: tokio::sync::broadcast::Receiver<(String, ThreadId, AgentEvent)>,
    thread_id: ThreadId,
    storage: S,
}

impl<S> Harness<S>
where
    S: SessionStorage + Clone + 'static,
{
    async fn start_with_spec(
        storage: S,
        spec: AgentSpec,
        provider: TestProvider,
        approval: ToolApprovalMode,
        initial_task: &str,
    ) -> Self {
        Self::start_with_team(storage, spec, vec![], provider, approval, initial_task).await
    }

    async fn start_with_team(
        storage: S,
        root: AgentSpec,
        subagents: Vec<AgentSpec>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        initial_task: &str,
    ) -> Self {
        let agents = AgentTeam::new(root, subagents).expect("valid team").build(
            ".",
            std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
        );
        Self::start_agents(storage, agents, provider, approval, initial_task).await
    }

    async fn start_agents(
        storage: S,
        agents: HashMap<String, Agent>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        initial_task: &str,
    ) -> Self {
        let config = test_config(provider, approval);

        let thread_id = ThreadId::new();
        let mut runtime = AgentRuntime::new(
            storage.clone(),
            thread_id.as_ref().to_string(),
            std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
        );
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

    async fn send_task(&self, task: &str) {
        self.runtime
            .send_message(user_task(&self.thread_id, task))
            .await
            .expect("send task");
    }

    async fn send_resume(
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
    /// agents that suspended in the previous run.
    async fn restart(
        &self,
        agents: HashMap<String, Agent>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        resume_decisions: HashMap<String, ResumeDecision>,
    ) -> Self {
        let config = test_config(provider, approval);

        let session_id = self.thread_id.as_ref().to_string();
        let snapshot: Option<AgentRuntimeSnapshot> = self
            .storage
            .load_session_snapshot(&session_id)
            .await
            .unwrap_or_default()
            .map(Into::into);

        let mut runtime = AgentRuntime::new(
            self.storage.clone(),
            session_id.clone(),
            std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
        );
        let events = runtime.subscribe();
        runtime
            .bootstrap(agents, snapshot, resume_decisions, config)
            .await;

        Self {
            runtime,
            events,
            thread_id: ThreadId(session_id),
            storage: self.storage.clone(),
        }
    }

    async fn next_event(&mut self) -> (String, ThreadId, AgentEvent) {
        self.events.recv().await.expect("receive event")
    }

    async fn shutdown(&self) {
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

#[tokio::test]
async fn wait_for_exit_honors_timeout_and_completes_after_exit() {
    let agents = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "main-system".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team")
    .build(
        ".",
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );

    let config = test_config(TestProvider::default(), ToolApprovalMode::Auto);

    let mut runtime = AgentRuntime::new(
        MemoryStorage::default(),
        "test-session".into(),
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );
    runtime
        .bootstrap(agents, None, HashMap::new(), config)
        .await;

    assert!(!runtime.wait_for_exit(Some(Duration::from_millis(20))).await);

    runtime.request_exit().await;
    assert!(runtime.wait_for_exit(Some(Duration::from_secs(2))).await);
}

/// Returns `(root, subagents)` for a `coda` root delegating to a single
/// `explore` sub-agent that owns the `read_todos` tool.
fn explore_read_todos_specs(main_prompt: &str) -> (AgentSpec, Vec<AgentSpec>) {
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

async fn wait_for_completion_after_explore_reply(
    harness: &mut Harness<MemoryStorage>,
    require_resume: bool,
) {
    let mut approval_resumed = false;
    let mut saw_subagent_tool = false;
    let mut saw_parent_tool_reply = false;
    let mut observed = Vec::new();

    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            observed.push(format!("{} {:?}", agent_name, event));
            match (agent_name.as_str(), event) {
                ("explore", AgentEvent::Suspended(pending)) if require_resume => {
                    approval_resumed = true;
                    harness
                        .send_resume(
                            &pending.agent_name,
                            &pending.thread_id,
                            vec![(pending.calls[0].id.clone(), ToolCallResolution::Execute)],
                        )
                        .await;
                }
                ("explore", AgentEvent::ToolCallEnd(tool)) if tool.name == "read_todos" => {
                    saw_subagent_tool = true;
                }
                ("coda", AgentEvent::ToolCallEnd(tool)) if tool.name == "explore" => {
                    saw_parent_tool_reply = true;
                    assert!(matches!(tool.output, ToolOutput::Ok(ref s) if s == "explore done"));
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert!(
                        saw_subagent_tool,
                        "explore never finished its local tool call"
                    );
                    assert!(
                        saw_parent_tool_reply,
                        "coda never received the explore reply"
                    );
                    assert_eq!(msg.content, "main done");
                    if require_resume {
                        assert!(approval_resumed, "explore was never resumed after approval");
                    }
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    if let Err(err) = result {
        panic!(
            "timed out waiting for explore completion: {err:?}; observed events: {}",
            observed.join(" | ")
        );
    }
}

#[tokio::test]
async fn stateless_subagent_replies_after_local_tool_execution() {
    let (root, subagents) = explore_read_todos_specs("main-system");
    let mut harness = Harness::start_with_team(
        MemoryStorage::default(),
        root,
        subagents,
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;
    wait_for_completion_after_explore_reply(&mut harness, false).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn stateless_subagent_replies_after_approval_resume() {
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let (root, subagents) = explore_read_todos_specs("main-system");
    let team = AgentTeam::new(root, subagents).expect("valid team");
    let agents1 = team.build(
        ".",
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        agents1,
        provider.clone(),
        approval.clone(),
        "inspect",
    )
    .await;

    // Phase 1: consume events until the explore subagent suspends for approval.
    let (pending, mut saw_subagent_tool) = {
        let result = timeout(Duration::from_secs(2), async {
            let mut saw_subagent_tool = false;
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                match (agent_name.as_str(), event) {
                    ("explore", AgentEvent::Suspended(pending)) => {
                        return (pending, saw_subagent_tool);
                    }
                    ("explore", AgentEvent::ToolCallEnd(tool)) if tool.name == "read_todos" => {
                        saw_subagent_tool = true;
                    }
                    _ => {}
                }
            }
        })
        .await;
        result.expect("timed out waiting for explore suspension")
    };
    harness.shutdown().await;

    // Phase 2: restart with resume, verify completion.
    let mut decisions = HashMap::new();
    decisions.insert(
        pending.thread_id.clone(),
        ResumeDecision {
            resolutions: vec![(pending.calls[0].id.clone(), ToolCallResolution::Execute)],
        },
    );
    let agents2 = team.build(
        ".",
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );
    let mut harness = harness
        .restart(agents2, provider, approval, decisions)
        .await;

    let mut saw_parent_tool_reply = false;
    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("explore", AgentEvent::ToolCallEnd(tool)) if tool.name == "read_todos" => {
                    saw_subagent_tool = true;
                }
                ("coda", AgentEvent::ToolCallEnd(tool)) if tool.name == "explore" => {
                    saw_parent_tool_reply = true;
                    assert!(matches!(tool.output, ToolOutput::Ok(ref s) if s == "explore done"));
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert!(
                        saw_subagent_tool,
                        "explore never finished its local tool call"
                    );
                    assert!(
                        saw_parent_tool_reply,
                        "coda never received the explore reply"
                    );
                    assert_eq!(msg.content, "main done");
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "timed out waiting for completion after resume"
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn pending_approval_supports_mixed_resolutions() {
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "approval-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec), Box::new(EchoToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let agents1 = team.build(
        ".",
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        agents1,
        provider.clone(),
        approval.clone(),
        "inspect approvals",
    )
    .await;

    // Phase 1: consume until suspended, collect pending info.
    let (_pending_thread_id, decisions_map) = {
        let result = timeout(Duration::from_secs(2), async {
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                if let ("coda", AgentEvent::Suspended(pending)) = (agent_name.as_str(), event) {
                    assert_eq!(pending.calls.len(), 4);
                    let mut decisions = HashMap::new();
                    decisions.insert(
                        pending.thread_id.clone(),
                        ResumeDecision {
                            resolutions: vec![
                                ("call_exec".into(), ToolCallResolution::Execute),
                                (
                                    "call_resolved".into(),
                                    ToolCallResolution::Resolved(ToolOutput::Ok(
                                        "resolved-by-test".into(),
                                    )),
                                ),
                                (
                                    "call_rejected".into(),
                                    ToolCallResolution::Rejected {
                                        reason: Some("nope".into()),
                                    },
                                ),
                            ],
                        },
                    );
                    return (pending.thread_id, decisions);
                }
            }
        })
        .await;
        result.expect("timed out waiting for suspension")
    };
    harness.shutdown().await;
    let agents2 = team.build(
        ".",
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );
    harness = harness
        .restart(agents2, provider, approval, decisions_map)
        .await;

    // Phase 2: consume events after resume, verify outcomes.
    let mut saw_tool_end_ids = HashSet::new();
    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::ToolCallEnd(tool)) => {
                    saw_tool_end_ids.insert(tool.id);
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert_eq!(msg.content, "approval-flow-ok");
                    assert!(saw_tool_end_ids.contains("call_exec"));
                    assert!(saw_tool_end_ids.contains("call_resolved"));
                    assert!(saw_tool_end_ids.contains("call_rejected"));
                    assert!(saw_tool_end_ids.contains("call_auto"));
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(result.is_ok(), "timed out waiting for mixed approval flow");
    harness.shutdown().await;
}

#[tokio::test]
async fn reject_pending_approval_via_restart() {
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "interrupt-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let agents1 = team.build(
        ".",
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        agents1,
        provider.clone(),
        approval.clone(),
        "phase1",
    )
    .await;

    // Phase 1: consume until Suspended (read_todos needs approval).
    let pending = {
        let result = timeout(Duration::from_secs(2), async {
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                if let ("coda", AgentEvent::Suspended(p)) = (agent_name.as_str(), event) {
                    return p;
                }
            }
        })
        .await;
        result.expect("timed out waiting for suspension")
    };
    harness.shutdown().await;

    // Phase 2: reject the pending approval and restart.
    // The agent processes the rejection and continues with "phase1",
    // producing the final response.
    let mut reject_decisions = HashMap::new();
    let reject_ids: Vec<String> = pending.calls.iter().map(|c| c.id.clone()).collect();
    reject_decisions.insert(
        pending.thread_id.clone(),
        ResumeDecision {
            resolutions: reject_ids
                .into_iter()
                .map(|id| {
                    (
                        id,
                        ToolCallResolution::Rejected {
                            reason: Some("replaced by new task".into()),
                        },
                    )
                })
                .collect(),
        },
    );
    let agents2 = team.build(
        ".",
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );
    let mut harness = harness
        .restart(agents2, provider, approval, reject_decisions)
        .await;

    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert_eq!(msg.content, "interrupt-flow-ok");
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "timed out waiting for completion after reject"
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn restart_re_emits_pending_approval_with_original_suspended_at() {
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "interrupt-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let agents1 = team.build(
        ".",
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        agents1,
        provider.clone(),
        approval.clone(),
        "phase1",
    )
    .await;

    let first_pending = {
        let result = timeout(Duration::from_secs(2), async {
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                if let ("coda", AgentEvent::Suspended(p)) = (agent_name.as_str(), event) {
                    return p;
                }
            }
        })
        .await;
        result.expect("timed out waiting for first suspension")
    };
    harness.shutdown().await;

    let agents2 = team.build(
        ".",
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );
    let mut harness = harness
        .restart(agents2, provider, approval, HashMap::new())
        .await;

    let resumed_pending = {
        let result = timeout(Duration::from_secs(2), async {
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                if let ("coda", AgentEvent::Suspended(p)) = (agent_name.as_str(), event) {
                    return p;
                }
            }
        })
        .await;
        result.expect("timed out waiting for resumed suspension")
    };

    assert_eq!(resumed_pending.suspended_at, first_pending.suspended_at);
    harness.shutdown().await;
}

#[tokio::test]
async fn abort_during_mixed_tool_execution_aborts_local_and_subagent_calls() {
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_team(
        storage.clone(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "abort-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(SlowToolSpec {
                gate: Arc::new(Notify::new()),
            })],
            subagents: vec!["explore".into()],
        },
        vec![AgentSpec {
            name: "explore".into(),
            description: String::new(),
            system_prompt: "hold-subagent".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![],
            subagents: vec![],
        }],
        TestProvider::with_hold_subagent(Arc::new(Notify::new())),
        ToolApprovalMode::Auto,
        "abort",
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        let mut started = HashSet::new();
        let mut ended = HashSet::new();

        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::ToolCallStart(tool)) => {
                    started.insert(tool.id);
                    if started.contains("call_slow") && started.contains("call_explore") {
                        harness.runtime.request_abort().await;
                    }
                }
                ("coda", AgentEvent::ToolCallEnd(tool))
                    if matches!(tool.outcome, ToolCallOutcome::Aborted) =>
                {
                    ended.insert(tool.id);
                }
                ("coda", AgentEvent::Aborted(AbortedTarget::ToolCalls(ids))) => {
                    assert!(ids.contains(&"call_slow".to_string()));
                    assert!(ids.contains(&"call_explore".to_string()));
                    // Every aborted ToolMessage written to history must have
                    // been announced via ToolCallEnd before the Aborted marker.
                    assert!(ended.contains("call_slow"));
                    assert!(ended.contains("call_explore"));
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    let checkpoint = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(checkpoint) = harness.storage.checkpoint(&harness.thread_id).await
                && matches!(
                    checkpoint.resume_point,
                    crate::persist::StoredResumePoint::Generation
                )
            {
                break checkpoint;
            }
            yield_now().await;
        }
    })
    .await
    .expect("checkpoint was not saved after abort");

    harness.shutdown().await;
    result.expect("timed out waiting for abort event");
    assert!(matches!(
        tool_message(&checkpoint.messages, "call_slow"),
        Some(tool) if matches!(tool.outcome, ToolCallOutcome::Aborted)
    ));
    assert!(matches!(
        tool_message(&checkpoint.messages, "call_explore"),
        Some(tool) if matches!(tool.outcome, ToolCallOutcome::Aborted)
    ));
}

#[tokio::test]
async fn abort_settles_cancel_aware_tool_with_partial_output() {
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_spec(
        storage.clone(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "abort-cancel-aware-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(CancelAwareToolSpec)],
            subagents: vec![],
        },
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "abort cancel aware",
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        let mut saw_end = false;
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::ToolCallStart(tool)) if tool.id == "call_cancel" => {
                    harness.runtime.request_abort().await;
                }
                ("coda", AgentEvent::ToolCallEnd(tool)) if tool.id == "call_cancel" => {
                    // The tool observed the cancellation and settled itself:
                    // its salvaged partial output is recorded, not the generic
                    // interruption message.
                    assert!(matches!(tool.outcome, ToolCallOutcome::Aborted));
                    assert!(matches!(
                        &tool.output,
                        ToolOutput::Err(reason) if reason.contains("partial output from teardown")
                    ));
                    saw_end = true;
                }
                ("coda", AgentEvent::Aborted(AbortedTarget::ToolCalls(ids))) => {
                    assert!(ids.contains(&"call_cancel".to_string()));
                    assert!(saw_end, "ToolCallEnd must precede the Aborted marker");
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    let checkpoint = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(checkpoint) = harness.storage.checkpoint(&harness.thread_id).await
                && matches!(
                    checkpoint.resume_point,
                    crate::persist::StoredResumePoint::Generation
                )
            {
                break checkpoint;
            }
            yield_now().await;
        }
    })
    .await
    .expect("checkpoint was not saved after abort");

    harness.shutdown().await;
    result.expect("timed out waiting for abort event");
    assert!(matches!(
        tool_message(&checkpoint.messages, "call_cancel"),
        Some(tool) if matches!(tool.outcome, ToolCallOutcome::Aborted)
            && matches!(
                &tool.output,
                ToolOutput::Err(reason) if reason.contains("partial output from teardown")
            )
    ));
}

#[tokio::test]
async fn abort_during_generation_emits_aborted_and_persists_partial_message() {
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_spec(
        storage.clone(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "abort-generation-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        TestProvider::with_hold_generation(Arc::new(Notify::new())),
        ToolApprovalMode::Auto,
        "abort generation",
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        let mut saw_chunk = false;
        let mut saw_reasoning = false;
        let mut saw_aborted_llm_end = false;

        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::LLMReasoningChunk(chunk)) => {
                    assert_eq!(chunk, "partial reasoning");
                    saw_reasoning = true;
                }
                ("coda", AgentEvent::LLMContentChunk(chunk)) => {
                    assert_eq!(chunk, "partial");
                    saw_chunk = true;
                    harness.runtime.request_abort().await;
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.aborted => {
                    assert!(msg.content.contains("partial"));
                    saw_aborted_llm_end = true;
                }
                ("coda", AgentEvent::Aborted(AbortedTarget::Generation)) => {
                    assert!(
                        saw_chunk,
                        "generation was aborted before any partial content"
                    );
                    assert!(
                        saw_reasoning,
                        "generation was aborted before any partial reasoning"
                    );
                    // The aborted partial message written to history must have
                    // been announced via LLMEnd before the Aborted marker.
                    assert!(
                        saw_aborted_llm_end,
                        "no LLMEnd was emitted for the aborted partial message"
                    );
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    let checkpoint = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(checkpoint) = harness.storage.checkpoint(&harness.thread_id).await
                && let Some(Message::Assistant(message)) = checkpoint.messages.last()
                && message.aborted
            {
                break checkpoint;
            }
            yield_now().await;
        }
    })
    .await
    .expect("checkpoint was not saved after generation abort");

    harness.shutdown().await;
    result.expect("timed out waiting for generation abort");
    assert!(matches!(
        checkpoint.resume_point,
        crate::persist::StoredResumePoint::Generation
    ));
    assert!(matches!(
        checkpoint.messages.last(),
        Some(Message::Assistant(message))
            if message.aborted
                && message.content.contains("partial")
                && message.content.contains("interrupted by the user")
                && message.reasoning_content.as_deref() == Some("partial reasoning")
    ));
}

#[tokio::test]
async fn llm_errors_surface_for_root_agent_and_reply_to_parent_agent() {
    let mut root = Harness::start_with_spec(
        MemoryStorage::default(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "error-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "root error",
    )
    .await;

    let root_result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = root.next_event().await;
            if agent_name == "coda"
                && let AgentEvent::Error(err) = event
            {
                assert_eq!(err, "Invalid response: main boom");
                break;
            }
        }
    })
    .await;
    root.shutdown().await;
    root_result.expect("timed out waiting for root agent error");

    let mut parent = Harness::start_with_team(
        MemoryStorage::default(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "error-parent-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec!["explore".into()],
        },
        vec![AgentSpec {
            name: "explore".into(),
            description: String::new(),
            system_prompt: "error-subagent".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![],
            subagents: vec![],
        }],
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "subagent error",
    )
    .await;

    let parent_result = timeout(Duration::from_secs(2), async {
        let mut saw_explore_error = false;

        loop {
            let (agent_name, _, event) = parent.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::ToolCallEnd(tool)) if tool.name == "explore" => {
                    saw_explore_error = true;
                    assert!(matches!(
                        tool.output,
                        ToolOutput::Err(ref out) if out == "Streaming error: subagent boom"
                    ));
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert!(
                        saw_explore_error,
                        "parent never received the subagent error"
                    );
                    assert_eq!(msg.content, "subagent-error-ok");
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    parent.shutdown().await;
    parent_result.expect("timed out waiting for subagent error reply");
}

#[tokio::test]
async fn user_task_is_checkpointed_before_turn_completes() {
    // The user prompt must be durable as soon as the turn starts — a mid-turn
    // crash or reconnect must not lose it. `abort-generation-main` holds the
    // LLM stream open, so observing the first chunk proves the turn is still
    // in flight when we inspect the checkpoint.
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_spec(
        storage.clone(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "abort-generation-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        TestProvider::with_hold_generation(Arc::new(Notify::new())),
        ToolApprovalMode::Auto,
        "hold this task",
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if agent_name == "coda" && matches!(event, AgentEvent::LLMContentChunk(_)) {
                break;
            }
        }
    })
    .await;
    result.expect("timed out waiting for generation to start");

    let checkpoint = harness
        .storage
        .checkpoint(&harness.thread_id)
        .await
        .expect("user task was not checkpointed at turn start");
    assert!(matches!(
        checkpoint.messages.last(),
        Some(Message::User(user)) if user.first_text() == Some("hold this task")
    ));
    assert!(matches!(
        checkpoint.resume_point,
        crate::persist::StoredResumePoint::Generation
    ));

    harness.shutdown().await;
}

#[tokio::test]
async fn new_task_while_suspended_emits_tool_call_end_for_discarded_calls() {
    // A new Task while suspended for approval writes aborted ToolMessages to
    // history (stale-envelope cleanup); each must be announced via ToolCallEnd
    // so event consumers stay consistent with the checkpoint.
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "interrupt-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let agents = team.build(
        ".",
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        agents,
        provider,
        approval,
        "phase1",
    )
    .await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::Suspended(_)) = (agent_name.as_str(), event) {
                break;
            }
        }
    })
    .await
    .expect("timed out waiting for suspension");

    // Send a fresh task instead of resuming: the pending call is discarded.
    harness.send_task("phase1").await;

    let result = timeout(Duration::from_secs(2), async {
        let mut saw_discarded_call = false;
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::ToolCallEnd(tool))
                    if tool.id == "call_approve"
                        && matches!(tool.outcome, ToolCallOutcome::Aborted) =>
                {
                    saw_discarded_call = true;
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert_eq!(msg.content, "interrupt-flow-ok");
                    assert!(
                        saw_discarded_call,
                        "no ToolCallEnd was emitted for the discarded pending call"
                    );
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    result.expect("timed out waiting for turn completion after new task");
    harness.shutdown().await;
}

#[tokio::test]
async fn in_process_resume_after_suspension() {
    // Verify that after an agent suspends for approval, sending a Resume
    // envelope in-process (without shutdown/restart) allows the turn to
    // complete normally.
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "interrupt-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let provider = TestProvider::default();
    let approval =
        ToolApprovalMode::RequireWhen(Arc::new(|call: &ToolCall| call.name == "read_todos"));
    let agents = team.build(
        ".",
        std::sync::Arc::new(coda_tools::BackgroundProcesses::new()),
    );
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        agents,
        provider,
        approval,
        "phase1",
    )
    .await;

    // Wait for the Suspended event.
    let pending = {
        let result = timeout(Duration::from_secs(2), async {
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                if let ("coda", AgentEvent::Suspended(p)) = (agent_name.as_str(), event) {
                    return p;
                }
            }
        })
        .await;
        result.expect("timed out waiting for suspension")
    };

    // Resume in-process — no shutdown/restart.
    harness
        .send_resume(
            &pending.agent_name,
            &pending.thread_id,
            vec![(pending.calls[0].id.clone(), ToolCallResolution::Execute)],
        )
        .await;

    // Verify the turn completes after in-process resume.
    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::LLMEnd(msg)) = (agent_name.as_str(), event)
                && msg.tool_calls.is_empty()
            {
                assert_eq!(msg.content, "interrupt-flow-ok");
                return;
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "timed out waiting for completion after in-process resume"
    );

    harness.shutdown().await;
}

/// Background-task notices ride in at the next user turn, ahead of the user's
/// message: the `TaskNotice` event precedes `LLMStart` and carries the exact
/// message object persisted in the checkpoint, the model sees notice-then-user
/// order, and delivery drains the registry (exactly once).
#[tokio::test]
async fn task_notices_inject_ahead_of_the_user_message() {
    let storage = TestStorage::default();
    let background = Arc::new(coda_tools::BackgroundProcesses::new());
    background
        .spawn_with(
            coda_tools::TaskMeta {
                command: "true".into(),
                description: "quick".into(),
                agent_name: "coda".into(),
            },
            |_ctx| async { coda_tools::TaskExit::Exited { code: Some(0) } },
        )
        .await
        .expect("spawn fake task");
    // Wait for the terminal commit so the notice is pending before the turn.
    let mut summaries = background.summaries();
    while summaries
        .borrow_and_update()
        .iter()
        .any(|summary| summary.status.is_running())
    {
        summaries.changed().await.expect("summaries watch");
    }

    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "notice-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let agents = team.build(".", background.clone());

    let thread_id = ThreadId::new();
    let mut runtime = AgentRuntime::new(
        storage.clone(),
        thread_id.as_ref().to_string(),
        background.clone(),
    );
    let mut events = runtime.subscribe();
    runtime
        .bootstrap(
            agents,
            None,
            HashMap::new(),
            test_config(TestProvider::default(), ToolApprovalMode::Auto),
        )
        .await;
    runtime
        .send_message(user_task(&thread_id, "hello"))
        .await
        .expect("send task");

    // Injection happens while handling the Task envelope, before the request
    // is built: TaskNotice must arrive ahead of LLMStart.
    let notice_message = loop {
        let (_, _, event) = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for TaskNotice")
            .expect("event stream open");
        match event {
            AgentEvent::TaskNotice(message) => break message,
            AgentEvent::LLMStart(_) => panic!("TaskNotice must precede LLMStart"),
            _ => {}
        }
    };
    assert!(matches!(
        &notice_message.origin,
        coda_core::llm::UserOrigin::TaskNotice { task_ids } if task_ids.len() == 1
    ));
    let notice_text = notice_message.first_text().expect("notice text");
    assert!(notice_text.contains("Background task bg_"), "{notice_text}");
    assert!(notice_text.contains("exited with code 0"), "{notice_text}");

    // The model saw the notice ahead of the user message.
    let content = loop {
        let (_, _, event) = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for LLMEnd")
            .expect("event stream open");
        if let AgentEvent::LLMEnd(message) = event {
            break message.content;
        }
    };
    assert!(content.starts_with("seen:Background task bg_"), "{content}");
    assert!(content.ends_with("|hello"), "{content}");

    // Checkpoint history holds the identical notice message, ahead of the
    // user's, so fold-from-events and checkpoint restore agree field by field.
    let checkpoint = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(checkpoint) = storage.checkpoint(&thread_id).await
                && checkpoint.messages.len() >= 3
            {
                break checkpoint;
            }
            yield_now().await;
        }
    })
    .await
    .expect("timed out waiting for the final checkpoint");
    let users: Vec<_> = checkpoint
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user),
            _ => None,
        })
        .collect();
    assert_eq!(users.len(), 2, "notice + user message");
    assert_eq!(
        serde_json::to_string(users[0]).expect("serialize"),
        serde_json::to_string(&notice_message).expect("serialize"),
        "checkpointed notice differs from the emitted event's message"
    );
    assert_eq!(users[1].first_text(), Some("hello"));

    // Exactly-once within the normal lifecycle: the registry is drained.
    assert!(background.take_notices().await.is_empty());

    runtime.request_exit().await;
    assert!(runtime.wait_for_exit(Some(Duration::from_secs(2))).await);
}
