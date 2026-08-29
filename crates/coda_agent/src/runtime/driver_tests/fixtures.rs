//! Shared fixtures for the driver tests: local tools, a fake LLM provider
//! scripted by system prompt, storage stand-ins, and the `Harness` that drives
//! an `AgentRuntime` and lets a test await its events.

use super::super::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, ModelProfile, RunConfig, Sender, StoredCheckpoint,
    StoredRuntimeSnapshot, SubAgentMode, ToolApprovalMode, ToolCallResolution,
    runtime::{
        AgentRuntime, AgentRuntimeSnapshot, ResumeTarget, SessionStorage, StoredResumePoint,
    },
};
use coda_core::{
    llm::{
        AssistantMessage, ChatCompletionRequest, CompletionUsage, LLMProvider, LLMStreamEvent,
        MessageId, ReasoningContinuation, RequestMessage, StreamError, ToolCall, ToolMessage,
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

/// A throwaway background-task registry, for the driver tests that only need
/// `AgentTeam::build` to have one. Each call gets its own, so nothing leaks
/// between tests.
pub(super) fn test_registry() -> std::sync::Arc<coda_background::BackgroundProcesses> {
    std::sync::Arc::new(coda_background::BackgroundProcesses::temporary())
}

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

/// The agent whose checkpoint writes are parked, and the gate that frees them.
type HeldWrites = Arc<Mutex<Option<(String, Arc<Notify>)>>>;

#[derive(Clone, Default)]
pub(super) struct TestStorage {
    checkpoints: Arc<Mutex<HashMap<String, StoredCheckpoint>>>,
    snapshots: Arc<Mutex<HashMap<String, StoredRuntimeSnapshot>>>,
    held: HeldWrites,
    /// Writes still allowed through before every later one fails.
    budget: Arc<Mutex<Option<usize>>>,
    fail_loads: Arc<Mutex<bool>>,
}

impl TestStorage {
    pub(super) async fn checkpoint(&self, thread_id: &ThreadId) -> Option<StoredCheckpoint> {
        self.checkpoints
            .lock()
            .await
            .get(thread_id.as_ref())
            .cloned()
    }

    /// Let the next `writes` checkpoint writes through, then fail every one
    /// after that — which is how a test aims a failure at one specific write
    /// point rather than at whichever write happens to come first.
    pub(super) async fn fail_checkpoints_after(&self, writes: usize) {
        *self.budget.lock().await = Some(writes);
    }

    pub(super) async fn fail_checkpoint_loads(&self) {
        *self.fail_loads.lock().await = true;
    }

    /// Park `agent_name`'s checkpoint writes until the returned gate is
    /// released. Lets a test prove that whoever is supposed to wait for a
    /// write really does wait for it, instead of racing a fast one.
    pub(super) async fn hold_checkpoints_of(&self, agent_name: &str) -> WriteGate {
        let open = Arc::new(Notify::new());
        *self.held.lock().await = Some((agent_name.to_string(), open.clone()));
        WriteGate {
            held: self.held.clone(),
            open,
        }
    }
}

/// Checkpoint writes parked by [`TestStorage::hold_checkpoints_of`].
pub(super) struct WriteGate {
    held: HeldWrites,
    open: Arc<Notify>,
}

impl WriteGate {
    /// Let the parked write land, and stop holding later ones.
    pub(super) async fn release(&self) {
        self.held.lock().await.take();
        self.open.notify_one();
    }
}

impl SessionStorage for TestStorage {
    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let spent = {
                let mut budget = self.budget.lock().await;
                match budget.as_mut() {
                    Some(0) => true,
                    Some(remaining) => {
                        *remaining -= 1;
                        false
                    }
                    None => false,
                }
            };
            if spent {
                return Err("storage is unavailable".to_string());
            }
            let open = match &*self.held.lock().await {
                Some((agent, open)) if *agent == checkpoint.agent_name => Some(open.clone()),
                _ => None,
            };
            if let Some(open) = open {
                open.notified().await;
            }
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
            if *self.fail_loads.lock().await {
                return Err("checkpoint load is unavailable".to_string());
            }
            let checkpoint = self.checkpoints.lock().await.get(&thread_id).cloned();
            Ok(checkpoint)
        })
    }

    fn load_pending_approval_checkpoints(
        &self,
        _session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<StoredCheckpoint>, String>> + Send + '_>> {
        Box::pin(async move {
            if *self.fail_loads.lock().await {
                return Err("checkpoint load is unavailable".to_string());
            }
            let mut checkpoints: Vec<_> = self
                .checkpoints
                .lock()
                .await
                .values()
                .filter(|checkpoint| {
                    matches!(
                        checkpoint.resume_point,
                        StoredResumePoint::PendingApproval {
                            ref pending_approval_calls,
                            ..
                        } if !pending_approval_calls.is_empty()
                    )
                })
                .cloned()
                .collect();
            checkpoints.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
            Ok(checkpoints)
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
    recorded_compactions: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    /// When `true`, the next compaction request fails and the flag flips
    /// back to `false` — scripts exactly one failure before later attempts
    /// succeed.
    fail_next_compaction: Option<Arc<std::sync::atomic::AtomicBool>>,
    recorded_requests: Option<Arc<std::sync::Mutex<Vec<ChatCompletionRequest>>>>,
}

impl TestProvider {
    pub(super) fn with_hold_subagent(hold_subagent: Arc<Notify>) -> Self {
        Self {
            hold_subagent: Some(hold_subagent),
            ..Self::default()
        }
    }

    pub(super) fn with_hold_generation(hold_generation: Arc<Notify>) -> Self {
        Self {
            hold_generation: Some(hold_generation),
            ..Self::default()
        }
    }

    pub(super) fn with_fail_next_compaction(flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            fail_next_compaction: Some(flag),
            ..Self::default()
        }
    }

    pub(super) fn with_recorded_compactions(requests: Arc<std::sync::Mutex<Vec<String>>>) -> Self {
        Self {
            recorded_compactions: Some(requests),
            ..Self::default()
        }
    }

    pub(super) fn with_recorded_requests(
        requests: Arc<std::sync::Mutex<Vec<ChatCompletionRequest>>>,
    ) -> Self {
        Self {
            recorded_requests: Some(requests),
            ..Self::default()
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
        if let Some(requests) = &self.recorded_requests {
            requests.lock().unwrap().push(request.clone());
        }
        let system_prompt = request
            .messages
            .first()
            .and_then(|message| match message {
                RequestMessage::System(system) => Some(system.0.as_str()),
                _ => None,
            })
            .unwrap_or_default();

        // A compaction request, not a turn: its system message is the
        // compaction prompt.
        if system_prompt.starts_with("You are compacting") {
            if let Some(requests) = &self.recorded_compactions {
                requests
                    .lock()
                    .expect("recorded compaction mutex poisoned")
                    .push(format!("{request:?}"));
            }
            if let Some(flag) = &self.fail_next_compaction
                && flag.swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Self::errored(StreamError::InvalidResponse(
                    "scripted compaction failure".into(),
                ));
            }
            return Self::completed(AssistantMessage {
                content: "gist of the earlier turn".into(),
                ..assistant()
            });
        }

        match system_prompt {
            "ptc-descriptor" => Self::completed(AssistantMessage {
                content: "done".into(),
                ..assistant()
            }),
            "ptc-list" => {
                if tool_message(&request.messages, "ptc_list").is_none() {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "ptc_list".into(),
                            name: coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME.into(),
                            arguments: Some("{}".into()),
                        }],
                        ..assistant()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        content: "done".into(),
                        ..assistant()
                    })
                }
            }
            "ptc-run" => {
                if tool_message(&request.messages, "ptc_call").is_none() {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "ptc_call".into(),
                            name: coda_tools::RUN_JAVASCRIPT_TOOL_NAME.into(),
                            arguments: Some(
                                serde_json::json!({
                                    "code": "return await tools.read_todos({});"
                                })
                                .to_string(),
                            ),
                        }],
                        ..assistant()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        content: "done".into(),
                        ..assistant()
                    })
                }
            }
            "auto-compact-first-turn-main" => match last_user(&request.messages) {
                Some("only") if tool_message(&request.messages, "call_first").is_none() => {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_first".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        }],
                        usage: Some(CompletionUsage {
                            total_tokens: 5_000,
                            ..Default::default()
                        }),
                        ..assistant()
                    })
                }
                Some(summary) if summary.starts_with("[compacted automatically:") => {
                    Self::completed(AssistantMessage {
                        content: "only done".into(),
                        usage: Some(CompletionUsage {
                            total_tokens: 100,
                            ..Default::default()
                        }),
                        ..assistant()
                    })
                }
                other => panic!("unexpected first-turn user state: {other:?}"),
            },
            // Answers "first" directly (low usage), then on "second" makes
            // two over-threshold tool calls before answering — giving a test
            // three `Generation` entries to check auto-compaction against.
            "auto-compact-main" => match last_user(&request.messages) {
                Some("first") => Self::completed(AssistantMessage {
                    content: "first done".into(),
                    usage: Some(CompletionUsage {
                        total_tokens: 100,
                        ..Default::default()
                    }),
                    ..assistant()
                }),
                Some("second") if tool_message(&request.messages, "call_1").is_none() => {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_1".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        }],
                        usage: Some(CompletionUsage {
                            total_tokens: 5_000,
                            ..Default::default()
                        }),
                        ..assistant()
                    })
                }
                Some("second") if tool_message(&request.messages, "call_2").is_none() => {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_2".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        }],
                        usage: Some(CompletionUsage {
                            total_tokens: 6_000,
                            ..Default::default()
                        }),
                        ..assistant()
                    })
                }
                Some("second") => Self::completed(AssistantMessage {
                    content: "second done".into(),
                    usage: Some(CompletionUsage {
                        total_tokens: 200,
                        ..Default::default()
                    }),
                    ..assistant()
                }),
                Some(summary) if summary.starts_with("[compacted automatically:") => {
                    Self::completed(AssistantMessage {
                        content: "second done".into(),
                        usage: Some(CompletionUsage {
                            total_tokens: 200,
                            ..Default::default()
                        }),
                        ..assistant()
                    })
                }
                other => panic!("unexpected user state: {other:?}"),
            },
            // Like "auto-compact-main", but the generation right after the
            // compaction attempt fails outright, so a test can check that the
            // next turn's auto-compaction check doesn't repeat what already
            // succeeded.
            "auto-compact-fail-then-continue-main" => match last_user(&request.messages) {
                Some("first") => Self::completed(AssistantMessage {
                    content: "first done".into(),
                    usage: Some(CompletionUsage {
                        total_tokens: 100,
                        ..Default::default()
                    }),
                    ..assistant()
                }),
                Some("second") if tool_message(&request.messages, "call_1").is_none() => {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_1".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        }],
                        usage: Some(CompletionUsage {
                            total_tokens: 5_000,
                            ..Default::default()
                        }),
                        ..assistant()
                    })
                }
                Some("second") => Self::errored(StreamError::InvalidResponse(
                    "simulated provider failure".into(),
                )),
                Some("third") => Self::completed(AssistantMessage {
                    content: "third done".into(),
                    usage: Some(CompletionUsage {
                        total_tokens: 100,
                        ..Default::default()
                    }),
                    ..assistant()
                }),
                other => panic!("unexpected user state: {other:?}"),
            },
            // A root that delegates once per turn to a stateful "explore",
            // which itself goes over threshold on its second invocation —
            // exercises auto-compaction on a sub-agent thread too.
            "auto-compact-subagent-main" => match last_user(&request.messages) {
                Some("first") if tool_message(&request.messages, "call_explore_1").is_none() => {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_explore_1".into(),
                            name: "agent__explore".into(),
                            arguments: Some(r#"{"task":"first"}"#.into()),
                        }],
                        usage: Some(CompletionUsage {
                            total_tokens: 100,
                            ..Default::default()
                        }),
                        ..assistant()
                    })
                }
                Some("first") => Self::completed(AssistantMessage {
                    content: "first done".into(),
                    usage: Some(CompletionUsage {
                        total_tokens: 100,
                        ..Default::default()
                    }),
                    ..assistant()
                }),
                Some("second") if tool_message(&request.messages, "call_explore_2").is_none() => {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_explore_2".into(),
                            name: "agent__explore".into(),
                            arguments: Some(r#"{"task":"second"}"#.into()),
                        }],
                        usage: Some(CompletionUsage {
                            total_tokens: 100,
                            ..Default::default()
                        }),
                        ..assistant()
                    })
                }
                Some("second") => Self::completed(AssistantMessage {
                    content: "second done".into(),
                    usage: Some(CompletionUsage {
                        total_tokens: 100,
                        ..Default::default()
                    }),
                    ..assistant()
                }),
                other => panic!("unexpected user state: {other:?}"),
            },
            "auto-compact-subagent-explore" => {
                let invocation = request
                    .messages
                    .iter()
                    .filter(|message| matches!(message, RequestMessage::User(_)))
                    .count();
                match invocation {
                    1 => Self::completed(AssistantMessage {
                        content: "explore round 1 done".into(),
                        usage: Some(CompletionUsage {
                            total_tokens: 100,
                            ..Default::default()
                        }),
                        ..assistant()
                    }),
                    2 if tool_message(&request.messages, "call_sub").is_none() => {
                        Self::completed(AssistantMessage {
                            tool_calls: vec![ToolCall {
                                id: "call_sub".into(),
                                name: "read_todos".into(),
                                arguments: Some("{}".into()),
                            }],
                            usage: Some(CompletionUsage {
                                total_tokens: 5_000,
                                ..Default::default()
                            }),
                            ..assistant()
                        })
                    }
                    2 => Self::completed(AssistantMessage {
                        content: "explore round 2 done".into(),
                        usage: Some(CompletionUsage {
                            total_tokens: 100,
                            ..Default::default()
                        }),
                        ..assistant()
                    }),
                    other => panic!("unexpected explore invocation count: {other}"),
                }
            }
            "main-system" => {
                let has_explore_result = request.messages.iter().any(
                    |message| matches!(message, RequestMessage::Tool(tool) if tool.name == "explore"),
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
                        |message| matches!(message, RequestMessage::Tool(tool) if tool.name == "explore"),
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
            // A root that answers straight away — no tools, no sub-agents.
            "plain-main" => Self::completed(AssistantMessage {
                content: "main done".into(),
                ..assistant()
            }),
            // A middle layer: calls its own sub-agent, then answers.
            "nested-explore" => {
                let has_probe_result = request.messages.iter().any(
                    |message| matches!(message, RequestMessage::Tool(tool) if tool.name == "probe"),
                );
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
                    |message| matches!(message, RequestMessage::Tool(tool) if tool.name == "read_todos"),
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
            // Two approval-gated calls in sequence, each in its own assistant
            // message, so a test can answer the first batch while the second is
            // the one actually parked. Both deliberately reuse one call id —
            // legal, since an id only has to be unique within its own assistant
            // message, and precisely what makes ids useless for telling the two
            // batches apart.
            "two-batch-approval" => {
                let answered = request
                    .messages
                    .iter()
                    .filter(
                        |message| matches!(message, RequestMessage::Tool(tool) if tool.name == "read_todos"),
                    )
                    .count();
                if answered < 2 {
                    Self::completed(AssistantMessage {
                        tool_calls: vec![ToolCall {
                            id: "call_1".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        }],
                        ..assistant()
                    })
                } else {
                    Self::completed(AssistantMessage {
                        content: "two-batch-done".into(),
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
                            let RequestMessage::Assistant(assistant) = message else {
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

fn last_user(messages: &[RequestMessage]) -> Option<&str> {
    messages.iter().rev().find_map(|message| match message {
        RequestMessage::User(user) => user.first_text(),
        _ => None,
    })
}

pub(super) fn tool_message<'a>(
    messages: &'a [RequestMessage],
    id: &str,
) -> Option<&'a ToolMessage> {
    messages.iter().find_map(|message| match message {
        RequestMessage::Tool(tool) if tool.id == id => Some(tool),
        _ => None,
    })
}

fn describe_tools(messages: &[RequestMessage]) -> String {
    let mut tools = messages
        .iter()
        .filter_map(|message| match message {
            RequestMessage::Tool(tool) => {
                Some(format!("{}:{:?}:{:?}", tool.id, tool.outcome, tool.output))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    tools.sort();
    tools.join("|")
}

pub(super) fn user_task(thread_id: &ThreadId, task: &str) -> Envelope {
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
            author: Default::default(),
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
            // Effectively disabled; auto-compaction tests build their own.
            auto_compact_threshold_tokens: u32::MAX,
        },
        agent_models: HashMap::new(),
        tool_approval: approval,
        approval_timeout: None,
    }
}

pub(super) struct Harness<S> {
    pub(super) runtime: AgentRuntime,
    events: tokio::sync::broadcast::Receiver<(String, ThreadId, TurnId, AgentEvent)>,
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
        let agents = AgentTeam::new(root, subagents).expect("valid team").build(
            ".",
            coda_tools::shared_file_locks(),
            test_registry(),
        );
        Self::start_agents(storage, agents, provider, approval, initial_task).await
    }

    pub(super) async fn start_agents(
        storage: S,
        agents: HashMap<String, Agent>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        initial_task: &str,
    ) -> Self {
        Self::start_with_config(
            storage,
            agents,
            test_config(provider, approval),
            initial_task,
        )
        .await
    }

    /// Like [`Self::start_agents`], but for a test that needs to shape the
    /// `RunConfig` itself (an auto-compaction threshold, say).
    pub(super) async fn start_with_config(
        storage: S,
        agents: HashMap<String, Agent>,
        config: RunConfig<TestProvider>,
        initial_task: &str,
    ) -> Self {
        Self::start_with_config_at(storage, agents, config, ThreadId::new(), initial_task).await
    }

    pub(super) async fn start_with_config_at(
        storage: S,
        agents: HashMap<String, Agent>,
        config: RunConfig<TestProvider>,
        thread_id: ThreadId,
        initial_task: &str,
    ) -> Self {
        let mut runtime = AgentRuntime::new(storage.clone(), thread_id.as_ref().to_string());
        runtime
            .bootstrap(agents, None, HashMap::new(), config)
            .await
            .expect("bootstrap");

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

    /// Answer a suspension exactly as a client would: addressed to the thread
    /// that announced it, naming the batch it announced. Sending the same
    /// `approval` twice is therefore a faithful duplicate submit.
    pub(super) async fn send_resume(
        &self,
        approval: &PendingApproval,
        resolutions: Vec<(String, ToolCallResolution)>,
    ) {
        self.runtime
            .send_message(Envelope::with_id(|id| Envelope {
                id,
                from: Sender::User,
                to: Receiver {
                    name: approval.agent_name.clone(),
                    thread_id: ThreadId::from(approval.thread_id.clone()),
                },
                reply_to: None,
                body: EnvelopeBody::Resume(crate::ResumeDecision {
                    parent_message_id: approval.parent_message_id,
                    resolutions,
                }),
            }))
            .await
            .expect("resume agent");
    }

    /// Restart the harness from storage, injecting resume decisions for agents
    /// that suspended in the previous run (keyed by agent name, carrying the
    /// thread each one is parked on — what `Session::open` derives from the
    /// pending approvals it collected).
    pub(super) async fn restart(
        &self,
        agents: HashMap<String, Agent>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        resume_targets: HashMap<String, (String, ResumeDecision)>,
    ) -> Self {
        let snapshot: Option<AgentRuntimeSnapshot> = self
            .storage
            .load_session_snapshot(self.thread_id.as_ref())
            .await
            .unwrap_or_default()
            .map(Into::into);
        self.restart_from(agents, provider, approval, resume_targets, snapshot)
            .await
    }

    /// Restart as if the previous process had died without any agent exiting:
    /// the checkpoints are on disk but no runtime snapshot was ever written. A
    /// session a fork just minted starts out in exactly this state.
    pub(super) async fn restart_without_snapshot(
        &self,
        agents: HashMap<String, Agent>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        resume_targets: HashMap<String, (String, ResumeDecision)>,
    ) -> Self {
        self.restart_from(agents, provider, approval, resume_targets, None)
            .await
    }

    async fn restart_from(
        &self,
        agents: HashMap<String, Agent>,
        provider: TestProvider,
        approval: ToolApprovalMode,
        resume_targets: HashMap<String, (String, ResumeDecision)>,
        snapshot: Option<AgentRuntimeSnapshot>,
    ) -> Self {
        let config = test_config(provider, approval);
        let session_id = self.thread_id.as_ref().to_string();
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
            .await
            .expect("bootstrap");

        Self {
            runtime,
            events,
            thread_id: ThreadId(session_id),
            storage: self.storage.clone(),
        }
    }

    /// The turn tag is dropped here: almost every test cares about who emitted
    /// what, not which submission it belonged to.
    pub(super) async fn next_event(&mut self) -> (String, ThreadId, AgentEvent) {
        let (agent_name, thread_id, _turn, event) =
            self.events.recv().await.expect("receive event");
        (agent_name, thread_id, event)
    }

    pub(super) async fn shutdown(&self) {
        // Cancel in-flight work first (e.g. a subagent blocked on a hold gate)
        // so the graceful exit below has something that can finish. This is
        // teardown, so it deliberately does not mark any turn as stopped.
        self.runtime.cancel_in_flight().await;
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
