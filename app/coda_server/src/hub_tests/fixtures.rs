//! Shared fixtures for the hub integration tests: a fake LLM provider, a
//! storage wrapper that can stall a chosen thread's checkpoint write, and a
//! `SessionOpener` built on both, plus the small helpers every test category
//! uses to drive a hub and wait on its state.

use super::super::*;
use coda_agent::persist::StoredRuntimeSnapshot;
use coda_agent::runtime::{MemoryStorage, SessionStorage};
use coda_agent::{
    AgentSpec, AgentTeam, ModelProfile, RunConfig, SubAgentMode, ThreadId, ToolApprovalMode,
};
use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, CompactionMessage, CompactionOutcome, CompletionUsage,
    LLMProvider, LLMStreamEvent, RequestMessage, StreamError, ToolCall,
};
use coda_tools::ReadTodosToolSpec;
use futures::{Stream, StreamExt, stream};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};

#[derive(Clone)]
pub(super) struct TestProvider {
    gate: Arc<Notify>,
}

impl TestProvider {
    fn completed(
        message: AssistantMessage,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>> {
        Box::pin(stream::iter(vec![Ok(LLMStreamEvent::Completed(Box::new(
            message,
        )))]))
    }
}

impl LLMProvider for TestProvider {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        let system = request
            .messages
            .first()
            .and_then(|m| match m {
                RequestMessage::System(s) => Some(s.0.clone()),
                _ => None,
            })
            .unwrap_or_default();
        // A compaction request, not a turn: system message is the compaction prompt.
        if system.starts_with("You are compacting") {
            return Self::completed(assistant("gist of the earlier turn"));
        }
        match system.as_str() {
            "reply" => Self::completed(assistant("done")),
            "read-task-results" | "read-running-task-result" => {
                let read_count = request
                    .messages
                    .iter()
                    .filter(|m| matches!(m, RequestMessage::Tool(_)))
                    .count();
                let expected_reads = if system == "read-running-task-result" {
                    2
                } else {
                    1
                };
                if read_count >= expected_reads {
                    Self::completed(assistant("read both terminal results"))
                } else {
                    let ids: Vec<String> = request
                        .messages
                        .iter()
                        .find_map(|m| match m {
                            RequestMessage::User(user) => {
                                Some(serde_json::from_str(user.first_text().unwrap()).unwrap())
                            }
                            _ => None,
                        })
                        .unwrap();
                    let gate = self.gate.clone();
                    Box::pin(stream::once(async move {
                        gate.notified().await;
                        let mut answer = assistant("");
                        answer.tool_calls = ids
                            .into_iter()
                            .enumerate()
                            .map(|(i, id)| ToolCall {
                                id: format!("read_{read_count}_{i}"),
                                name: "task_output".into(),
                                arguments: Some(serde_json::json!({"id":id}).to_string()),
                            })
                            .collect();
                        Ok(LLMStreamEvent::Completed(Box::new(answer)))
                    }))
                }
            }
            "hold" => {
                let gate = self.gate.clone();
                Box::pin(
                    stream::iter(vec![Ok(LLMStreamEvent::ContentChunk("partial".into()))]).chain(
                        stream::once(async move {
                            gate.notified().await;
                            Ok(LLMStreamEvent::Completed(Box::new(assistant("final"))))
                        }),
                    ),
                )
            }
            // 200 chunks: comfortably within the broadcast channel's capacity
            // (256), so even a fully starved pump cannot lag — the buffer
            // holds the whole burst. (A real LLM stream awaits the network
            // per chunk so the producer yields; this synchronous iter is
            // already an adversarial case.)
            "burst" => {
                let chunks: Vec<_> = (0..200)
                    .map(|i| Ok(LLMStreamEvent::ContentChunk(format!("c{i} "))))
                    .collect();
                Box::pin(stream::iter(chunks).chain(Self::completed(assistant("burst done"))))
            }
            // One turn that fans out far more local tool calls than
            // `RelayConfig::default().max_message_tier_events` — each
            // completion is a message-tier `ToolCallEnd`, so this turn must
            // trip the forced-resync path long before it would ever settle.
            "runaway" => {
                let has_result = request
                    .messages
                    .iter()
                    .any(|m| matches!(m, RequestMessage::Tool(t) if t.name == "read_todos"));
                if has_result {
                    Self::completed(assistant("completed after live resync"))
                } else {
                    let mut msg = assistant("");
                    msg.tool_calls = (0..(RelayConfig::default().max_message_tier_events + 10))
                        .map(|i| ToolCall {
                            id: format!("call_{i}"),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        })
                        .collect();
                    Self::completed(msg)
                }
            }
            // One call out to the `explore` sub-agent, then an answer. The
            // replacement turn a rewind starts says "different" and is answered
            // straight away, so the sub-agent's thread is only ever written by
            // the turn being discarded — which is what lets a test read that
            // thread afterwards and know whose messages it is looking at.
            "delegate" => {
                let replacement = request.messages.iter().any(
                    |m| matches!(m, RequestMessage::User(user) if user.first_text() == Some("different")),
                );
                let answered = request
                    .messages
                    .iter()
                    .any(|m| matches!(m, RequestMessage::Tool(_)));
                if replacement || answered {
                    Self::completed(assistant("done"))
                } else {
                    let mut msg = assistant("");
                    msg.tool_calls = vec![ToolCall {
                        id: "call_explore".into(),
                        name: "explore".into(),
                        arguments: Some(r#"{"task":"look"}"#.into()),
                    }];
                    Self::completed(msg)
                }
            }
            "approval" => {
                let has_result = request
                    .messages
                    .iter()
                    .any(|m| matches!(m, RequestMessage::Tool(t) if t.name == "read_todos"));
                if has_result {
                    Self::completed(assistant("approved-done"))
                } else {
                    let mut msg = assistant("");
                    msg.tool_calls = vec![ToolCall {
                        id: "call_todos".into(),
                        name: "read_todos".into(),
                        arguments: Some("{}".into()),
                    }];
                    Self::completed(msg)
                }
            }
            // Turn 2 crosses threshold after its own tool call, compacting turn 1.
            "auto-compact" => {
                let user_count = request
                    .messages
                    .iter()
                    .filter(|m| matches!(m, RequestMessage::User(_)))
                    .count();
                let has_tool_result = request
                    .messages
                    .iter()
                    .any(|m| matches!(m, RequestMessage::Tool(_)));
                match (user_count, has_tool_result) {
                    (1, _) => {
                        let mut msg = assistant("first done");
                        msg.usage = Some(CompletionUsage {
                            total_tokens: 100,
                            ..Default::default()
                        });
                        Self::completed(msg)
                    }
                    (2, false) => {
                        let mut msg = assistant("");
                        msg.tool_calls = vec![ToolCall {
                            id: "call_1".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        }];
                        msg.usage = Some(CompletionUsage {
                            total_tokens: 5_000,
                            ..Default::default()
                        });
                        Self::completed(msg)
                    }
                    (2, true) => {
                        let mut msg = assistant("second done");
                        msg.usage = Some(CompletionUsage {
                            total_tokens: 100,
                            ..Default::default()
                        });
                        Self::completed(msg)
                    }
                    other => panic!("unexpected auto-compact request shape: {other:?}"),
                }
            }
            // Like "approval", but blocks before returning the tool call, so a
            // test can detach mid-turn and have the suspension itself settle
            // while genuinely unattended.
            "approval_hold" => {
                let gate = self.gate.clone();
                Box::pin(
                    stream::iter(vec![Ok(LLMStreamEvent::ContentChunk("partial".into()))]).chain(
                        stream::once(async move {
                            gate.notified().await;
                            let mut msg = assistant("");
                            msg.tool_calls = vec![ToolCall {
                                id: "call_todos".into(),
                                name: "read_todos".into(),
                                arguments: Some("{}".into()),
                            }];
                            Ok(LLMStreamEvent::Completed(Box::new(msg)))
                        }),
                    ),
                )
            }
            other => panic!("unexpected system prompt: {other}"),
        }
    }
}

/// `MemoryStorage` that can be told to drag its feet on one thread's checkpoint
/// writes, or to refuse them outright.
///
/// A sub-agent saves before it replies, and its caller cannot get past the call
/// without that reply — so stalling the write stalls the whole turn. Widening
/// that on purpose is what makes the ordering observable instead of something a
/// test would only catch by luck.
#[derive(Clone, Default)]
pub(super) struct SlowStorage {
    inner: MemoryStorage,
    stall: Option<(String, Duration)>,
    /// Writes still allowed through before every later one fails.
    budget: Arc<tokio::sync::Mutex<Option<usize>>>,
}

impl SlowStorage {
    /// Let the next `writes` checkpoint writes through, then fail every one
    /// after that.
    pub(super) async fn fail_checkpoints_after(&self, writes: usize) {
        *self.budget.lock().await = Some(writes);
    }
}

impl SessionStorage for SlowStorage {
    fn has_notice_receipt(
        &self,
        task_id: coda_core::task::TaskId,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + '_>> {
        self.inner.has_notice_receipt(task_id)
    }

    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: coda_agent::persist::StoredCheckpoint,
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
            if let Some((slow_thread, delay)) = &self.stall
                && slow_thread == &thread_id
            {
                tokio::time::sleep(*delay).await;
            }
            self.inner.save_checkpoint(thread_id, checkpoint).await
        })
    }

    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<coda_agent::persist::StoredCheckpoint>, String>>
                + Send
                + '_,
        >,
    > {
        self.inner.load_checkpoint(thread_id)
    }

    fn load_pending_approval_checkpoints(
        &self,
        session_id: &str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<coda_agent::persist::StoredCheckpoint>, String>>
                + Send
                + '_,
        >,
    > {
        self.inner.load_pending_approval_checkpoints(session_id)
    }

    fn save_session_snapshot(
        &self,
        session_id: String,
        snapshot: coda_agent::persist::StoredRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.save_session_snapshot(session_id, snapshot)
    }

    fn load_session_snapshot(
        &self,
        session_id: &str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<coda_agent::persist::StoredRuntimeSnapshot>, String>>
                + Send
                + '_,
        >,
    > {
        self.inner.load_session_snapshot(session_id)
    }
}

/// Every `mark_unseen_outcome` / `clear_unseen_outcome` call a `TestOpener`
/// recorded, in order: `Some(outcome)` for a mark, `None` for a clear.
pub(super) type UnseenOutcomeCalls = Vec<(SessionKey, Option<UnseenOutcome>)>;

pub(super) struct TestOpener {
    pub(super) storage: SlowStorage,
    /// Spool root for this test's sessions; dropped with the opener.
    background_root: tempfile::TempDir,
    provider: TestProvider,
    team: AgentTeam,
    approval: ToolApprovalMode,
    pub(super) fail_effort_update: bool,
    /// Fail the rebuild a rewind performs after its truncation has committed.
    pub(super) fail_open_after_rewind: bool,
    rewound: Arc<std::sync::atomic::AtomicBool>,
    /// The cuts `fork` was asked for, in order, with what the gate said about
    /// the source.
    pub(super) forks: Arc<std::sync::Mutex<Vec<(ForkCut, ForkSource)>>>,
    /// Holds `fork` inside the storage call until released, so a test can drive
    /// another command while the copy is in flight.
    pub(super) fork_gate: Option<Arc<Notify>>,
    pub(super) fork_error: Option<ForkError>,
    /// The mode cells handed to each session this opener built, in order.
    /// A runtime reads its posture through the cell for as long as it lives, so
    /// a test can read one back to see what the *running* session would now
    /// decide — which is how the live-switch path is checked without a rebuild.
    pub(super) opened_modes: Arc<std::sync::Mutex<Vec<PermissionModeCell>>>,
    /// Holds `compact` until released, so a test can drive other commands while
    /// the summary is notionally in flight — which is the whole point of the
    /// entry lock being free during one.
    pub(super) compact_gate: Option<Arc<Notify>>,
    /// What `compact` reports. `Ok(true)` is a summary that moved the boundary,
    /// `Ok(false)` a recorded failure that did not.
    pub(super) compact_result: Result<bool, CompactError>,
    pub(super) unseen_outcomes: Arc<std::sync::Mutex<UnseenOutcomeCalls>>,
    /// Holds `mark_unseen_outcome` until released, so a test can drive a
    /// concurrent `attach` while the entry lock is still held across it.
    pub(super) mark_unseen_gate: Option<Arc<Notify>>,
    /// Notified as soon as `mark_unseen_outcome` is entered, before it waits
    /// on `mark_unseen_gate` — a rendezvous point for tests.
    pub(super) mark_unseen_entered: Arc<Notify>,
    /// Effectively disabled by default; lowered by tests that need it.
    pub(super) auto_compact_threshold_tokens: u32,
    /// Holds `delete_persisted` until released, so a test can drive an attach
    /// while the delete tombstone is still standing.
    pub(super) delete_gate: Option<Arc<Notify>>,
    /// Notified as soon as `delete_persisted` is entered, before it waits on
    /// `delete_gate` — a rendezvous point for tests.
    pub(super) delete_entered: Arc<Notify>,
    /// What `delete_persisted` reports; `Some` makes it fail, leaving the
    /// session behind for a later attach to reopen.
    pub(super) delete_error: Option<String>,
    /// The keys `delete_persisted` was asked for, in order.
    pub(super) deleted: Arc<std::sync::Mutex<Vec<SessionKey>>>,
    /// `"open"` / `"delete_persisted"` in call order. Opening a session is
    /// where the real opener (re-)creates its stored row, so the order of these
    /// two is what says whether a delete can pull that row out from under a
    /// session that is already live.
    pub(super) calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl TestOpener {
    pub(super) fn new(system_prompt: &str, approval: ToolApprovalMode) -> Self {
        let tools: Vec<Box<dyn coda_tools::ToolSpec>> = if matches!(
            system_prompt,
            "approval" | "approval_hold" | "runaway" | "auto-compact"
        ) {
            vec![Box::new(ReadTodosToolSpec)]
        } else {
            vec![]
        };
        Self::with_team(
            AgentTeam::new(
                AgentSpec {
                    name: "coda".into(),
                    description: String::new(),
                    system_prompt: system_prompt.into(),
                    mode: SubAgentMode::Stateful,
                    tools,
                    subagents: vec![],
                },
                vec![],
            )
            .expect("valid team"),
            approval,
            SlowStorage::default(),
        )
    }

    /// A root that delegates one call to a stateful `explore` sub-agent, so the
    /// session spans two threads. `stall` delays `explore`'s checkpoint writes.
    pub(super) fn delegating(explore_prompt: &str, stall: Option<Duration>) -> Self {
        let team = AgentTeam::new(
            AgentSpec {
                name: "coda".into(),
                description: String::new(),
                system_prompt: "delegate".into(),
                mode: SubAgentMode::Stateful,
                tools: vec![],
                subagents: vec!["explore".into()],
            },
            vec![AgentSpec {
                name: "explore".into(),
                description: String::new(),
                system_prompt: explore_prompt.into(),
                mode: SubAgentMode::Stateful,
                tools: vec![],
                subagents: vec![],
            }],
        )
        .expect("valid team");
        Self::with_team(
            team,
            ToolApprovalMode::Auto,
            SlowStorage {
                inner: MemoryStorage::default(),
                stall: stall.map(|delay| (explore_thread().as_ref().to_string(), delay)),
                budget: Arc::default(),
            },
        )
    }

    fn with_team(team: AgentTeam, approval: ToolApprovalMode, storage: SlowStorage) -> Self {
        Self {
            storage,
            background_root: tempfile::tempdir().expect("temp spool root"),
            provider: TestProvider {
                gate: Arc::new(Notify::new()),
            },
            team,
            approval,
            fail_effort_update: false,
            fail_open_after_rewind: false,
            rewound: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            forks: Arc::new(std::sync::Mutex::new(Vec::new())),
            fork_gate: None,
            fork_error: None,
            opened_modes: Arc::new(std::sync::Mutex::new(Vec::new())),
            compact_gate: None,
            compact_result: Ok(true),
            unseen_outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            mark_unseen_gate: None,
            mark_unseen_entered: Arc::new(Notify::new()),
            auto_compact_threshold_tokens: u32::MAX,
            delete_gate: None,
            delete_entered: Arc::new(Notify::new()),
            delete_error: None,
            deleted: Arc::new(std::sync::Mutex::new(Vec::new())),
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

/// The thread the `explore` sub-agent runs in: stateful, so it is derived once
/// from the root thread (whose id is the session id) and stays put.
pub(super) fn explore_thread() -> ThreadId {
    ThreadId::from_uuid5(&ThreadId::from(key().1), "explore")
}

impl SessionOpener for TestOpener {
    fn open<'a>(
        &'a self,
        key: &'a SessionKey,
        _provider_id: &'a str,
        _reasoning_effort: Option<String>,
        permission_mode: PermissionModeCell,
        decisions: HashMap<String, ResumeDecision>,
        background: Option<Arc<coda_process::BackgroundTasks>>,
    ) -> Pin<Box<dyn Future<Output = Result<Session, OpenError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("calls mutex poisoned")
                .push("open");
            self.opened_modes
                .lock()
                .unwrap()
                .push(permission_mode.clone());
            let after_rewind = self.rewound.load(std::sync::atomic::Ordering::SeqCst);
            if after_rewind && self.fail_open_after_rewind {
                return Err(OpenError::Storage("injected rebuild failure".into()));
            }
            let session = Session::builder()
                .storage(self.storage.clone())
                .team(&self.team, ".")
                .run_config(RunConfig {
                    default_model: ModelProfile {
                        provider: self.provider.clone(),
                        model: "fake".into(),
                        label: "fake".into(),
                        temperature: None,
                        max_completion_tokens: None,
                        reasoning_effort: None,
                        auto_compact_threshold_tokens: self.auto_compact_threshold_tokens,
                    },
                    agent_models: HashMap::new(),
                    tool_approval: self.approval.clone(),
                    approval_timeout: None,
                })
                .session_id(key.1.clone())
                .resume_decisions(decisions)
                .background(background)
                .open()
                .await?;
            Ok(session)
        })
    }

    fn background_archive(&self, key: &SessionKey) -> Result<coda_process::ArchiveDir, String> {
        let dir = self.background_root.path().join(&key.0).join(&key.1);
        coda_process::ArchiveDir::open_or_create_root(&dir).map_err(|e| e.to_string())
    }

    /// Stands in for the SQL delete plus the spool removal. `MemoryStorage`
    /// has no delete, so the stored conversation is left alone — what these
    /// tests are about is *when* this runs relative to attach, not what SQL it
    /// spells. The spool is real, so a test can see that it is gone.
    fn delete_persisted<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("calls mutex poisoned")
                .push("delete_persisted");
            self.delete_entered.notify_one();
            if let Some(gate) = &self.delete_gate {
                gate.notified().await;
            }
            self.deleted
                .lock()
                .expect("deleted mutex poisoned")
                .push(key.clone());
            if let Some(error) = &self.delete_error {
                return Err(error.clone());
            }
            let dir = self.background_root.path().join(&key.0).join(&key.1);
            if let Err(error) = std::fs::remove_dir_all(&dir)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error.to_string());
            }
            Ok(())
        })
    }

    fn load_messages<'a>(
        &'a self,
        _key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Vec<Message>> + Send + 'a>> {
        Box::pin(async { vec![] })
    }

    fn compact<'a>(
        &'a self,
        _key: &'a SessionKey,
        _provider_id: &'a str,
        _reasoning_effort: Option<&'a str>,
        instructions: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Compacted, CompactError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(gate) = &self.compact_gate {
                gate.notified().await;
            }
            let applied = match &self.compact_result {
                Ok(applied) => *applied,
                Err(CompactError::Stale) => return Err(CompactError::Stale),
                Err(CompactError::Empty) => return Err(CompactError::Empty),
                Err(CompactError::InvalidHistory(reason)) => {
                    return Err(CompactError::InvalidHistory(reason.clone()));
                }
                Err(CompactError::Storage(reason)) => {
                    return Err(CompactError::Storage(reason.clone()));
                }
            };
            Ok(Compacted {
                command: Message::User(UserMessage::text(
                    MessageId::new(),
                    if instructions.is_empty() {
                        "/compact".to_string()
                    } else {
                        format!("/compact {instructions}")
                    },
                )),
                outcome: Message::Compaction(CompactionMessage {
                    message_id: MessageId::new(),
                    // A failure record is transcript-only, and no boundary.
                    outcome: if applied {
                        CompactionOutcome::Summary {
                            cutoff: MessageId::new(),
                        }
                    } else {
                        CompactionOutcome::Failed
                    },
                    content: "a summary".into(),
                    created_at: jiff::Timestamp::now(),
                }),
                applied,
            })
        })
    }

    /// A stand-in for the SQL one: same predicate (drop every turn the root
    /// thread carries from the target on, in every thread), same snapshot
    /// clearing. It does not drop emptied thread records — `MemoryStorage`
    /// cannot delete — which is fine here, because what these tests are about is
    /// *when* the truncation runs relative to the agents, not how it is spelled.
    fn rewind<'a>(
        &'a self,
        key: &'a SessionKey,
        target: MessageId,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Message>, RewindError>> + Send + 'a>> {
        Box::pin(async move {
            self.rewound
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let root = self
                .storage
                .load_checkpoint(&key.1)
                .await
                .map_err(RewindError::Persistence)?
                .ok_or(RewindError::TargetNotFound)?;
            let cut = root
                .messages
                .iter()
                .position(|entry| {
                    matches!(&entry.message, Message::User(user) if user.message_id == target)
                })
                .ok_or(RewindError::TargetNotFound)?;
            let discarded: HashSet<uuid::Uuid> = root.messages[cut..]
                .iter()
                .map(|entry| entry.turn_id.as_uuid())
                .collect();

            for mut checkpoint in self.storage.inner.all_checkpoints().await {
                checkpoint
                    .messages
                    .retain(|entry| !discarded.contains(&entry.turn_id.as_uuid()));
                let thread_id = checkpoint.thread_id.clone();
                self.storage
                    .save_checkpoint(thread_id, checkpoint)
                    .await
                    .map_err(RewindError::Persistence)?;
            }
            self.storage
                .save_session_snapshot(
                    key.1.clone(),
                    StoredRuntimeSnapshot {
                        drained_envelopes: HashMap::new(),
                        agent_drained_envelopes: HashMap::new(),
                        active_threads: HashMap::new(),
                    },
                )
                .await
                .map_err(RewindError::Persistence)?;

            Ok(root.messages[..cut]
                .iter()
                .map(|entry| entry.message.clone())
                .collect())
        })
    }

    /// A stand-in for the SQL one. These tests are about the gate around the
    /// copy — idleness, the attach race, the borrowed entry — not about how the
    /// copy itself is spelled, which `storage_pg` covers.
    fn fork<'a>(
        &'a self,
        _key: &'a SessionKey,
        cut: ForkCut,
        source: ForkSource,
    ) -> Pin<Box<dyn Future<Output = Result<ForkedSession, ForkError>> + Send + 'a>> {
        Box::pin(async move {
            self.forks
                .lock()
                .expect("forks mutex poisoned")
                .push((cut, source));
            if let Some(gate) = &self.fork_gate {
                gate.notified().await;
            }
            match &self.fork_error {
                Some(err) => Err(err.clone()),
                None => Ok(ForkedSession {
                    session_id: "forked-session".to_string(),
                    name: None,
                }),
            }
        })
    }

    fn update_reasoning_effort<'a>(
        &'a self,
        _key: &'a SessionKey,
        _provider_id: &'a str,
        _reasoning_effort: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        let fail = self.fail_effort_update;
        Box::pin(async move {
            if fail {
                Err("injected metadata write failure".to_string())
            } else {
                Ok(())
            }
        })
    }

    fn mark_unseen_outcome<'a>(
        &'a self,
        key: &'a SessionKey,
        outcome: UnseenOutcome,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.mark_unseen_entered.notify_one();
            if let Some(gate) = &self.mark_unseen_gate {
                gate.notified().await;
            }
            self.unseen_outcomes
                .lock()
                .expect("unseen_outcomes mutex poisoned")
                .push((key.clone(), Some(outcome)));
        })
    }

    fn clear_unseen_outcome<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.unseen_outcomes
                .lock()
                .expect("unseen_outcomes mutex poisoned")
                .push((key.clone(), None));
        })
    }
}

pub(super) fn hub_with(
    system_prompt: &str,
    approval: ToolApprovalMode,
) -> (SessionHub, Arc<Notify>) {
    let opener = Arc::new(TestOpener::new(system_prompt, approval));
    let gate = opener.provider.gate.clone();
    (SessionHub::new(opener, RelayConfig::default()), gate)
}

pub(super) fn hub_with_failing_metadata(system_prompt: &str) -> SessionHub {
    let mut opener = TestOpener::new(system_prompt, ToolApprovalMode::Auto);
    opener.fail_effort_update = true;
    SessionHub::new(Arc::new(opener), RelayConfig::default())
}

pub(super) fn key() -> SessionKey {
    ("ws".to_string(), "s1".to_string())
}

/// A hub whose opener stays reachable, for tests that assert on what the opener
/// was asked for or that need to open its gates.
pub(super) fn hub_and_opener(opener: TestOpener) -> (Arc<SessionHub>, Arc<TestOpener>) {
    let opener = Arc::new(opener);
    (
        Arc::new(SessionHub::new(opener.clone(), RelayConfig::default())),
        opener,
    )
}

/// Like `hub_and_opener`, but also hands back the provider's `hold` gate —
/// for tests that need both (e.g. inspecting `unseen_outcomes` while a
/// "hold" turn is deliberately kept in flight).
pub(super) fn hub_opener_and_gate(
    opener: TestOpener,
) -> (Arc<SessionHub>, Arc<TestOpener>, Arc<Notify>) {
    let gate = opener.provider.gate.clone();
    let (hub, opener) = hub_and_opener(opener);
    (hub, opener, gate)
}

/// Reach into the live state. Some of the windows these tests are about are
/// opened by the runtime for a few microseconds, which is not something the
/// public surface can hold still.
pub(super) async fn with_live<R>(hub: &SessionHub, f: impl FnOnce(&mut LiveState) -> R) -> R {
    let entry = hub.get_entry(&key()).expect("a live entry");
    let mut guard = entry.inner.clone().lock_owned().await;
    let EntryPhase::Live(live) = &mut guard.phase else {
        panic!("the entry is not live");
    };
    f(live)
}

/// The entry's background task registry, so a test can start and settle tasks
/// the way `shell` would.
pub(super) async fn background_of(hub: &SessionHub) -> Arc<coda_process::BackgroundTasks> {
    let entry = hub.get_entry(&key()).expect("a live entry");
    let guard = entry.inner.clone().lock_owned().await;
    guard
        .background
        .clone()
        .flatten()
        .expect("an initialized entry has a registry")
}

/// Where `key`'s background tasks spool under this opener's root. Tests use it
/// to watch a delete reach disk.
pub(super) fn spool_dir(opener: &TestOpener, key: &SessionKey) -> std::path::PathBuf {
    opener.background_root.path().join(&key.0).join(&key.1)
}

/// Metadata for a test task.
pub(super) fn task_meta(command: &str) -> coda_process::TaskMeta {
    coda_process::TaskMeta::shell(command.into(), "test task".into(), "coda".into())
}

/// Await the next `RelayEvent` matching `pred`, skipping others.
pub(super) async fn next_matching(
    events: &mut BoxStream<'static, RelayEvent>,
    pred: impl Fn(&RelayEvent) -> bool,
) -> RelayEvent {
    timeout(Duration::from_secs(5), async {
        loop {
            let event = events.next().await.expect("stream ended unexpectedly");
            if pred(&event) {
                return event;
            }
        }
    })
    .await
    .expect("timed out waiting for relay event")
}

pub(super) fn is_settling_llm_end(event: &RelayEvent) -> bool {
    matches!(
        event,
        RelayEvent::Event(e)
            if matches!(&**e, WireEvent::LlmEnd { message, .. } if message.tool_calls.is_empty())
    )
}

/// Wait until the forwarder has folded the settled turn.
///
/// A client is handed the settling event *before* the forwarder updates the
/// entry it came from, so a test that acts the moment that event arrives can
/// beat the hub to its own bookkeeping and see a session that is still marked
/// as running.
pub(super) async fn wait_idle(hub: &SessionHub) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Some(entry) = hub.get_entry(&key()) {
                let idle = {
                    let guard = entry.inner.clone().lock_owned().await;
                    matches!(&guard.phase, EntryPhase::Live(live) if !live.turn_running)
                };
                if idle {
                    return;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("session did not settle");
}

pub(super) async fn wait_released(hub: &SessionHub) {
    timeout(Duration::from_secs(5), async {
        loop {
            if hub.get_entry(&key()).is_none() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("entry was not released");
}

/// Pure helper shared with the event-log/fold unit tests below.
pub(super) fn assistant(content: &str) -> AssistantMessage {
    let now = jiff::Timestamp::now();
    AssistantMessage {
        message_id: MessageId::new(),
        content: content.into(),
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
