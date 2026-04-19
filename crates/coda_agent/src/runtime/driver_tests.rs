use super::*;
use crate::{
    AgentCheckpoint, AgentEvent, AgentSpec, AgentState, BuildContext, RunConfig, Sender,
    SubAgentMode, ToolApprovalMode, ToolCallResolution,
    runtime::{AgentRuntime, AgentRuntimeSnapshot, MemoryStorage, SessionStorage},
    spec::{ReadTodosToolSpec, ToolSpec},
};
use coda_core::{
    llm::{
        AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, Message, StreamError,
        ToolCall, ToolMessage,
    },
    tool::{Tool, ToolObject, ToolResult, ToolWrapper},
};
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

#[derive(Clone, Default)]
struct TestStorage {
    checkpoints: Arc<Mutex<HashMap<String, AgentCheckpoint>>>,
    snapshots: Arc<Mutex<HashMap<String, AgentRuntimeSnapshot>>>,
}

impl TestStorage {
    async fn checkpoint(&self, thread_id: &ThreadId) -> Option<AgentCheckpoint> {
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
        checkpoint: AgentCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.checkpoints.lock().await.insert(thread_id, checkpoint);
            Ok(())
        })
    }

    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentCheckpoint>, String>> + Send + '_>> {
        let thread_id = thread_id.to_owned();
        Box::pin(async move {
            let checkpoint = self.checkpoints.lock().await.get(&thread_id).cloned();
            Ok(checkpoint)
        })
    }

    fn save_session_snapshot(
        &self,
        session_id: String,
        snapshot: AgentRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.snapshots.lock().await.insert(session_id, snapshot);
            Ok(())
        })
    }

    fn load_session_snapshot(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentRuntimeSnapshot>, String>> + Send + '_>>
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
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        async move { Ok(params.text) }
    }
}

struct EchoToolSpec;

impl ToolSpec for EchoToolSpec {
    fn build(&self, _state: &Arc<Mutex<AgentState>>, _ctx: &BuildContext) -> Box<dyn ToolObject> {
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
    fn build(&self, _state: &Arc<Mutex<AgentState>>, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(SlowTool::new(self.gate.clone())))
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
            ..Default::default()
        }
    }

    fn with_hold_generation(hold_generation: Arc<Notify>) -> Self {
        Self {
            hold_generation: Some(hold_generation),
            ..Default::default()
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
        chunk: &str,
        gate: Arc<Notify>,
        final_message: AssistantMessage,
    ) -> Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>> {
        Box::pin(
            stream::iter(vec![Ok(LLMStreamEvent::ContentChunk(chunk.into()))]).chain(stream::once(
                async move {
                    gate.notified().await;
                    Ok(LLMStreamEvent::Completed(final_message))
                },
            )),
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
                        ..Default::default()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_explore".into(),
                            name: "explore".into(),
                            arguments: Some(r#"{"task":"inspect the crate"}"#.into()),
                        }],
                        ..Default::default()
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
                        ..Default::default()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_read_todos".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        }],
                        ..Default::default()
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
                        ..Default::default()
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
                        ..Default::default()
                    })
                }
            }
            "interrupt-main" => match last_user(&request.messages) {
                Some("phase1") => Self::completed(AssistantMessage {
                    tool_calls: vec![ToolCall {
                        id: "call_approve".into(),
                        name: "read_todos".into(),
                        arguments: Some("{}".into()),
                    }],
                    ..Default::default()
                }),
                Some("phase2")
                    if matches!(
                        tool_message(&request.messages, "call_approve"),
                        Some(tool) if matches!(tool.outcome, ToolCallOutcome::Aborted)
                    ) =>
                {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_explore".into(),
                            name: "explore".into(),
                            arguments: Some(r#"{"task":"hold"}"#.into()),
                        }],
                        ..Default::default()
                    })
                }
                Some("phase3") => {
                    let ok = matches!(
                        tool_message(&request.messages, "call_approve"),
                        Some(tool) if matches!(tool.outcome, ToolCallOutcome::Aborted)
                    ) && matches!(
                        tool_message(&request.messages, "call_explore"),
                        Some(tool) if matches!(tool.outcome, ToolCallOutcome::Aborted)
                    );

                    Self::completed(AssistantMessage {
                        content: if ok {
                            "interrupt-flow-ok".into()
                        } else {
                            format!("interrupt-flow-bad: {}", describe_tools(&request.messages))
                        },
                        ..Default::default()
                    })
                }
                other => Self::completed(AssistantMessage {
                    content: format!("unexpected-user-state: {other:?}"),
                    ..Default::default()
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
                ..Default::default()
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
                        ..Default::default()
                    }))
                }))
            }
            "abort-generation-main" => {
                let hold_generation = self
                    .hold_generation
                    .clone()
                    .expect("abort-generation-main prompt requires a notify");
                Self::chunk_then_wait(
                    "partial",
                    hold_generation,
                    AssistantMessage {
                        content: "should not complete".into(),
                        ..Default::default()
                    },
                )
            }
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
                        ..Default::default()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_explore".into(),
                            name: "explore".into(),
                            arguments: Some(r#"{"task":"inspect failure"}"#.into()),
                        }],
                        ..Default::default()
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
        Message::User(user) => Some(user.0.as_str()),
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
        body: EnvelopeBody::Task(task.into()),
    })
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
    async fn start(
        storage: S,
        spec: AgentSpec,
        provider: TestProvider,
        approval: ToolApprovalMode,
        initial_task: &str,
    ) -> Self {
        let agents = spec
            .build(&BuildContext {
                workspace_dir: ".".into(),
            })
            .expect("build agent tree");

        let config = RunConfig {
            provider,
            model: "fake".into(),
            temperature: None,
            max_completion_tokens: None,
            tool_approval: approval,
        };

        let thread_id = ThreadId::new();
        let mut runtime = AgentRuntime::new(storage.clone(), thread_id.as_ref().to_string());
        runtime.bootstrap(agents, None, config).await;

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
        checkpoint: &AgentCheckpoint,
        resolutions: Vec<(String, ToolCallResolution)>,
    ) {
        self.runtime
            .send_message(Envelope::with_id(|id| Envelope {
                id,
                from: Sender::User,
                to: Receiver {
                    name: checkpoint.agent_name.clone(),
                    thread_id: ThreadId::from(checkpoint.thread_id.clone()),
                },
                reply_to: None,
                body: EnvelopeBody::Resume(crate::ResumeDecision { resolutions }),
            }))
            .await
            .expect("resume agent");
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
    let agents = AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: "main-system".into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec![],
    }
    .build(&BuildContext {
        workspace_dir: ".".into(),
    })
    .expect("build agent tree");

    let config = RunConfig {
        provider: TestProvider::default(),
        model: "fake".into(),
        temperature: None,
        max_completion_tokens: None,
        tool_approval: ToolApprovalMode::Auto,
    };

    let mut runtime = AgentRuntime::new(MemoryStorage::default(), "test-session".into());
    runtime.bootstrap(agents, None, config).await;

    assert!(!runtime.wait_for_exit(Some(Duration::from_millis(20))).await);

    runtime.request_exit().await;
    assert!(runtime.wait_for_exit(Some(Duration::from_secs(2))).await);
}

fn explore_read_todos_spec(main_prompt: &str) -> AgentSpec {
    AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: main_prompt.into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec![AgentSpec {
            name: "explore".into(),
            description: String::new(),
            system_prompt: "explore-system".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        }],
    }
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
                ("explore", AgentEvent::Suspended(checkpoint)) if require_resume => {
                    let ResumePoint::PendingApproval {
                        pending_approval_calls,
                        ..
                    } = &checkpoint.resume_point
                    else {
                        panic!("unexpected checkpoint state");
                    };
                    approval_resumed = true;
                    harness
                        .send_resume(
                            &checkpoint,
                            vec![(
                                pending_approval_calls[0].id.clone(),
                                ToolCallResolution::Execute,
                            )],
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
    let mut harness = Harness::start(
        MemoryStorage::default(),
        explore_read_todos_spec("main-system"),
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
    let mut harness = Harness::start(
        MemoryStorage::default(),
        explore_read_todos_spec("main-system"),
        TestProvider::default(),
        ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos")),
        "inspect",
    )
    .await;
    wait_for_completion_after_explore_reply(&mut harness, true).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn pending_approval_supports_mixed_resolutions() {
    let mut harness = Harness::start(
        MemoryStorage::default(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "approval-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec), Box::new(EchoToolSpec)],
            subagents: vec![],
        },
        TestProvider::default(),
        ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos")),
        "inspect approvals",
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        let mut saw_tool_end_ids = HashSet::new();

        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::Suspended(checkpoint)) => {
                    let ResumePoint::PendingApproval {
                        pending_approval_calls,
                        ..
                    } = &checkpoint.resume_point
                    else {
                        panic!("unexpected checkpoint state");
                    };
                    assert_eq!(pending_approval_calls.len(), 4);
                    harness
                        .send_resume(
                            &checkpoint,
                            vec![
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
                        )
                        .await;
                }
                ("coda", AgentEvent::ToolCallEnd(tool)) => {
                    saw_tool_end_ids.insert(tool.id);
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert_eq!(msg.content, "approval-flow-ok");
                    assert!(saw_tool_end_ids.contains("call_exec"));
                    assert!(saw_tool_end_ids.contains("call_auto"));
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    harness.shutdown().await;
    result.expect("timed out waiting for mixed approval flow");
}

#[tokio::test]
async fn new_task_replaces_pending_approval_and_pending_reply() {
    let mut harness = Harness::start(
        MemoryStorage::default(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "interrupt-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![AgentSpec {
                name: "explore".into(),
                description: String::new(),
                system_prompt: "hold-subagent".into(),
                mode: SubAgentMode::Stateless,
                tools: vec![],
                subagents: vec![],
            }],
        },
        TestProvider::with_hold_subagent(Arc::new(Notify::new())),
        ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos")),
        "phase1",
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        let mut sent_phase2 = false;
        let mut sent_phase3 = false;

        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::Suspended(_)) if !sent_phase2 => {
                    sent_phase2 = true;
                    harness.send_task("phase2").await;
                }
                ("coda", AgentEvent::ToolCallStart(tool))
                    if tool.name == "explore" && !sent_phase3 =>
                {
                    sent_phase3 = true;
                    harness.send_task("phase3").await;
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert!(sent_phase2, "phase2 was never sent");
                    assert!(sent_phase3, "phase3 was never sent");
                    assert_eq!(msg.content, "interrupt-flow-ok");
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    harness.shutdown().await;
    result.expect("timed out waiting for interrupt flow");
}

#[tokio::test]
async fn abort_during_mixed_tool_execution_aborts_local_and_subagent_calls() {
    let storage = TestStorage::default();
    let mut harness = Harness::start(
        storage.clone(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "abort-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(SlowToolSpec {
                gate: Arc::new(Notify::new()),
            })],
            subagents: vec![AgentSpec {
                name: "explore".into(),
                description: String::new(),
                system_prompt: "hold-subagent".into(),
                mode: SubAgentMode::Stateless,
                tools: vec![],
                subagents: vec![],
            }],
        },
        TestProvider::with_hold_subagent(Arc::new(Notify::new())),
        ToolApprovalMode::Auto,
        "abort",
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        let mut started = HashSet::new();

        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::ToolCallStart(tool)) => {
                    started.insert(tool.id);
                    if started.contains("call_slow") && started.contains("call_explore") {
                        harness.runtime.request_abort().await;
                    }
                }
                ("coda", AgentEvent::Aborted(AbortedTarget::ToolCalls(ids))) => {
                    assert!(ids.contains(&"call_slow".to_string()));
                    assert!(ids.contains(&"call_explore".to_string()));
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
                && matches!(checkpoint.resume_point, ResumePoint::Generation)
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
async fn abort_during_generation_emits_aborted_and_persists_partial_message() {
    let storage = TestStorage::default();
    let mut harness = Harness::start(
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

        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::LLMContentChunk(chunk)) => {
                    assert_eq!(chunk, "partial");
                    saw_chunk = true;
                    harness.runtime.request_abort().await;
                }
                ("coda", AgentEvent::Aborted(AbortedTarget::Generation)) => {
                    assert!(
                        saw_chunk,
                        "generation was aborted before any partial content"
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
    assert!(matches!(checkpoint.resume_point, ResumePoint::Generation));
    assert!(matches!(
        checkpoint.messages.last(),
        Some(Message::Assistant(message))
            if message.aborted
                && message.content.contains("partial")
                && message.content.contains("interrupted by the user")
    ));
}

#[tokio::test]
async fn llm_errors_surface_for_root_agent_and_reply_to_parent_agent() {
    let mut root = Harness::start(
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

    let mut parent = Harness::start(
        MemoryStorage::default(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "error-parent-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![AgentSpec {
                name: "explore".into(),
                description: String::new(),
                system_prompt: "error-subagent".into(),
                mode: SubAgentMode::Stateless,
                tools: vec![],
                subagents: vec![],
            }],
        },
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
