use super::*;
use crate::notices::MemNoticeStore;
use coda_agent::runtime::MemoryStorage;
use coda_agent::{
    AgentSpec, AgentTeam, ModelProfile, RunConfig, SubAgentMode, ToolApprovalMode,
    ToolCallResolution,
};
use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, StreamError, ToolCall,
    ToolMessage, ToolOutput,
};
use coda_tools::ReadTodosToolSpec;
use coda_tools::{TaskExit, TaskMeta, TaskNotice, TaskStatus};
use futures::{Stream, StreamExt, stream};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};

// Canonical background task ids (bg_ + 32 hex) for notice/dedupe tests.
const BG_1: &str = "bg_00000000000000000000000000000001";
const BG_A: &str = "bg_0000000000000000000000000000000a";
const BG_B: &str = "bg_0000000000000000000000000000000b";
const BG_C: &str = "bg_0000000000000000000000000000000c";
const BG_NEW: &str = "bg_00000000000000000000000000000e0e";

// --- pure helpers -----------------------------------------------------

fn assistant(content: &str) -> AssistantMessage {
    let now = jiff::Timestamp::now();
    AssistantMessage {
        content: content.into(),
        tool_calls: vec![],
        usage: None,
        reasoning_content: None,
        reasoning_ended_at: None,
        aborted: false,
        started_at: now,
        ended_at: now,
    }
}

fn tool_message(id: &str, text: &str) -> ToolMessage {
    ToolMessage::new(
        id.to_string(),
        "echo".to_string(),
        ToolOutput::Ok(text.to_string()),
        coda_core::llm::ToolCallOutcome::Auto,
        None,
    )
}

fn llm_end(agent: &str, message: AssistantMessage) -> WireEvent {
    WireEvent::LlmEnd {
        agent_name: agent.into(),
        thread_id: "t".into(),
        message,
    }
}

fn tool_end(agent: &str, message: ToolMessage) -> WireEvent {
    WireEvent::ToolCallEnd {
        agent_name: agent.into(),
        thread_id: "t".into(),
        message,
    }
}

fn chunk(agent: &str, text: &str) -> WireEvent {
    WireEvent::LlmContentChunk {
        agent_name: agent.into(),
        thread_id: "t".into(),
        content: text.into(),
    }
}

fn user(text: &str) -> Message {
    Message::User(UserMessage::text(text.to_string()))
}

// --- EventLog ----------------------------------------------------------

#[test]
fn event_log_overflow_drops_oldest_chunk_tier_first() {
    let limits = RelayConfig::default();
    let mut log = EventLog::new(limits);
    for i in 0..limits.max_log_events {
        if i == 10 {
            log.push(tool_end("coda", tool_message("keep", "kept")));
        } else {
            log.push(chunk("coda", &format!("c{i}")));
        }
    }
    log.push(llm_end("coda", assistant("fin")));
    assert_eq!(log.entries.len(), limits.max_log_events);
    // The oldest chunk was evicted; the message-tier events survive.
    assert!(matches!(
        log.entries.front(),
        Some(WireEvent::LlmContentChunk { content, .. }) if content == "c1"
    ));
    assert!(
        log.iter()
            .any(|e| matches!(e, WireEvent::ToolCallEnd { message, .. } if message.id == "keep"))
    );
}

#[test]
fn event_log_all_message_tier_grows_past_chunk_cap() {
    // `push` itself never drops a message-tier entry — dropping one would
    // corrupt the fold. Bounding this case is `message_tier_overflowed`'s
    // job (checked below), enforced by the forwarder forcing a resync;
    // see `runaway_tool_calls_force_resync_instead_of_unbounded_log`.
    let limits = RelayConfig::default();
    let mut log = EventLog::new(limits);
    for i in 0..(limits.max_log_events + 5) {
        log.push(tool_end("coda", tool_message(&format!("m{i}"), "x")));
    }
    assert_eq!(log.entries.len(), limits.max_log_events + 5);
}

#[test]
fn event_log_message_tier_overflow_flag() {
    let limits = RelayConfig::default();
    let mut log = EventLog::new(limits);
    for i in 0..limits.max_message_tier_events {
        log.push(tool_end("coda", tool_message(&format!("m{i}"), "x")));
        assert!(!log.message_tier_overflowed());
    }
    log.push(tool_end("coda", tool_message("one_too_many", "x")));
    assert!(log.message_tier_overflowed());

    // Settling (which folds and clears the log) resets the count.
    log.clear();
    assert!(!log.message_tier_overflowed());
}

// --- fold_settled_turn ---------------------------------------------------

#[test]
fn fold_orders_stale_cleanup_before_user() {
    // History order on a stale-envelope turn: aborted ToolMessages first,
    // then the new user prompt, then the assistant reply.
    let mut snapshot = vec![];
    let mut users = VecDeque::from([user("new task")]);
    let mut log = EventLog::new(RelayConfig::default());
    log.push(tool_end("coda", tool_message("stale1", "aborted")));
    log.push(tool_end("coda", tool_message("stale2", "aborted")));
    log.push(chunk("coda", "hi"));
    log.push(llm_end("coda", assistant("reply")));

    fold_settled_turn(&mut snapshot, &mut users, &mut log, "coda");

    assert_eq!(snapshot.len(), 4);
    assert!(matches!(&snapshot[0], Message::Tool(t) if t.id == "stale1"));
    assert!(matches!(&snapshot[1], Message::Tool(t) if t.id == "stale2"));
    assert!(matches!(&snapshot[2], Message::User(_)));
    assert!(matches!(&snapshot[3], Message::Assistant(a) if a.content == "reply"));
    assert!(log.entries.is_empty());
    assert!(users.is_empty());
}

#[test]
fn fold_skips_subagent_and_chunk_events() {
    let mut snapshot = vec![];
    let mut users = VecDeque::from([user("task")]);
    let mut log = EventLog::new(RelayConfig::default());
    log.push(chunk("coda", "x"));
    log.push(llm_end("coda", assistant("delegating")));
    log.push(llm_end("explore", assistant("sub result")));
    log.push(tool_end("explore", tool_message("sub_call", "sub")));
    log.push(tool_end(
        "coda",
        tool_message("agent_call", "reply from sub"),
    ));
    log.push(llm_end("coda", assistant("done")));

    fold_settled_turn(&mut snapshot, &mut users, &mut log, "coda");

    // user, assistant(delegating), tool(agent_call), assistant(done)
    assert_eq!(snapshot.len(), 4);
    assert!(matches!(&snapshot[0], Message::User(_)));
    assert!(matches!(&snapshot[1], Message::Assistant(a) if a.content == "delegating"));
    assert!(matches!(&snapshot[2], Message::Tool(t) if t.id == "agent_call"));
    assert!(matches!(&snapshot[3], Message::Assistant(a) if a.content == "done"));
}

#[test]
fn fold_places_task_notices_between_stale_cleanup_and_user() {
    // Driver write order on a notice-carrying stale turn:
    // aborted ToolMessages → TaskNotice user messages → the human's message.
    let mut snapshot = vec![];
    let mut users = VecDeque::from([user("new task")]);
    let mut log = EventLog::new(RelayConfig::default());
    log.push(tool_end("coda", tool_message("stale", "aborted")));
    log.push(WireEvent::TaskNotice {
        agent_name: "coda".into(),
        thread_id: "t".into(),
        message: UserMessage::task_notice(
            "Background task bg_1 finished",
            vec![coda_core::llm::TaskNoticeKey::Completed {
                task_id: BG_1.to_string(),
            }],
        ),
    });
    log.push(chunk("coda", "hi"));
    log.push(llm_end("coda", assistant("reply")));

    fold_settled_turn(&mut snapshot, &mut users, &mut log, "coda");

    assert_eq!(snapshot.len(), 4);
    assert!(matches!(&snapshot[0], Message::Tool(t) if t.id == "stale"));
    assert!(matches!(
        &snapshot[1],
        Message::User(u) if matches!(
            &u.origin,
            UserOrigin::TaskNotice { notice_keys }
                if notice_keys == &vec![coda_core::llm::TaskNoticeKey::Completed {
                    task_id: BG_1.to_string(),
                }]
        )
    ));
    assert!(matches!(
        &snapshot[2],
        Message::User(u) if u.origin == UserOrigin::Human
    ));
    assert!(matches!(&snapshot[3], Message::Assistant(a) if a.content == "reply"));
}

#[test]
fn restored_notices_dedupe_against_checkpointed_deliveries() {
    use coda_core::llm::TaskNoticeKey;
    use coda_tools::TaskNoticeFact;

    // The checkpoint says bg_a's completion and bg_b's completion were already
    // delivered (by fact key).
    let history = vec![
        user("hello"),
        Message::User(UserMessage::task_notice(
            "delivered earlier",
            vec![
                TaskNoticeKey::Completed {
                    task_id: BG_A.to_string(),
                },
                TaskNoticeKey::Completed {
                    task_id: BG_B.to_string(),
                },
            ],
        )),
    ];
    let killed = TaskStatus::Killed {
        at: jiff::Timestamp::now(),
    };
    let full = |id: &str| TaskNotice::Task {
        id: id.parse().unwrap(),
        command: "x".into(),
        description: String::new(),
        status: killed.clone(),
        output_tail: String::new(),
        stdout_overwritten: 0,
        stderr_overwritten: 0,
    };
    let fact = |id: &str| TaskNoticeFact::Completed {
        id: id.parse().unwrap(),
        status: killed.clone(),
    };
    let restored = vec![
        full(BG_A), // duplicate completion — drop
        full(BG_NEW),
        TaskNotice::Overflow {
            batch_id: "batch-x".into(),
            dropped: vec![fact(BG_B), fact(BG_C)],
            uncounted: 2,
        },
    ];

    let deduped = dedupe_restored_notices(restored, &history);

    assert_eq!(deduped.len(), 2, "{deduped:?}");
    assert!(matches!(&deduped[0], TaskNotice::Task { id, .. } if id.as_str() == BG_NEW));
    assert!(matches!(
        &deduped[1],
        TaskNotice::Overflow { dropped, uncounted: 2, .. }
            if dropped.len() == 1 && matches!(&dropped[0], TaskNoticeFact::Completed { id, .. } if id.as_str() == BG_C)
    ));
}

#[test]
fn fold_tolerates_missing_user_for_resumed_turns() {
    let mut snapshot = vec![];
    let mut users = VecDeque::new();
    let mut log = EventLog::new(RelayConfig::default());
    log.push(tool_end("coda", tool_message("resolved", "ok")));
    log.push(llm_end("coda", assistant("after resume")));

    fold_settled_turn(&mut snapshot, &mut users, &mut log, "coda");

    assert_eq!(snapshot.len(), 2);
    assert!(matches!(&snapshot[0], Message::Tool(t) if t.id == "resolved"));
    assert!(matches!(&snapshot[1], Message::Assistant(_)));
}

// --- event_settles_turn --------------------------------------------------

#[test]
fn settle_ignores_aborted_llm_end() {
    let mut aborted = assistant("partial");
    aborted.aborted = true;
    assert!(!event_settles_turn(&llm_end("coda", aborted), "coda"));
    assert!(event_settles_turn(
        &llm_end("coda", assistant("done")),
        "coda"
    ));
    assert!(!event_settles_turn(
        &llm_end("explore", assistant("sub")),
        "coda"
    ));
    assert!(event_settles_turn(
        &WireEvent::Aborted {
            agent_name: "coda".into(),
            thread_id: "t".into(),
            target: crate::wire::AbortedTargetWire::Generation,
        },
        "coda"
    ));
}

// --- integration: hub over real sessions ---------------------------------

#[derive(Clone)]
struct TestProvider {
    gate: Arc<Notify>,
}

impl TestProvider {
    fn completed(
        message: AssistantMessage,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>> {
        Box::pin(stream::iter(vec![Ok(LLMStreamEvent::Completed(message))]))
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
                Message::System(s) => Some(s.0.clone()),
                _ => None,
            })
            .unwrap_or_default();
        match system.as_str() {
            "reply" => Self::completed(assistant("done")),
            "hold" => {
                let gate = self.gate.clone();
                Box::pin(
                    stream::iter(vec![Ok(LLMStreamEvent::ContentChunk("partial".into()))]).chain(
                        stream::once(async move {
                            gate.notified().await;
                            Ok(LLMStreamEvent::Completed(assistant("final")))
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
                    .any(|m| matches!(m, Message::Tool(t) if t.name == "read_todos"));
                if has_result {
                    Self::completed(assistant(
                        "should not settle: resync should have fired first",
                    ))
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
            "approval" => {
                let has_result = request
                    .messages
                    .iter()
                    .any(|m| matches!(m, Message::Tool(t) if t.name == "read_todos"));
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
            other => panic!("unexpected system prompt: {other}"),
        }
    }
}

struct TestOpener {
    storage: MemoryStorage,
    provider: TestProvider,
    team: AgentTeam,
    approval: ToolApprovalMode,
    /// One-shot injected failure for the next `open` call.
    fail_next_open: std::sync::atomic::AtomicBool,
    /// Stable on-disk root for per-key background archives, so a reopened entry
    /// recovers the same archive.
    archive_root: tempfile::TempDir,
}

impl TestOpener {
    fn new(system_prompt: &str, approval: ToolApprovalMode) -> Self {
        let tools: Vec<Box<dyn coda_tools::ToolSpec>> =
            if matches!(system_prompt, "approval" | "runaway") {
                vec![Box::new(ReadTodosToolSpec)]
            } else {
                vec![]
            };
        let team = AgentTeam::new(
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
        .expect("valid team");
        Self {
            storage: MemoryStorage::default(),
            provider: TestProvider {
                gate: Arc::new(Notify::new()),
            },
            team,
            approval,
            fail_next_open: std::sync::atomic::AtomicBool::new(false),
            archive_root: tempfile::tempdir().expect("temp archive root"),
        }
    }
}

impl SessionOpener for TestOpener {
    fn open<'a>(
        &'a self,
        key: &'a SessionKey,
        _provider_id: &'a str,
        _reasoning_effort: Option<ReasoningEffort>,
        decisions: HashMap<String, ResumeDecision>,
        background: Arc<BackgroundProcesses>,
    ) -> Pin<Box<dyn Future<Output = Result<Session, OpenError>> + Send + 'a>> {
        Box::pin(async move {
            if self
                .fail_next_open
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(OpenError::Storage("injected open failure".into()));
            }
            Session::builder()
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
                    },
                    agent_models: HashMap::new(),
                    tool_approval: self.approval.clone(),
                    approval_timeout: None,
                })
                .session_id(key.1.clone())
                .resume_decisions(decisions)
                .background(background)
                .open()
                .await
        })
    }

    fn load_messages<'a>(
        &'a self,
        _key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Vec<Message>> + Send + 'a>> {
        Box::pin(async { vec![] })
    }

    fn background_archive(&self, key: &SessionKey) -> Result<coda_tools::ArchiveDir, String> {
        let dir = self
            .archive_root
            .path()
            .join(&key.0)
            .join(&key.1)
            .join("background/tasks");
        coda_tools::ArchiveDir::open_or_create_root(&dir).map_err(|e| e.to_string())
    }
}

fn hub_with(system_prompt: &str, approval: ToolApprovalMode) -> (SessionHub, Arc<Notify>) {
    let (hub, gate, _) = hub_with_notices(system_prompt, approval);
    (hub, gate)
}

fn hub_with_notices(
    system_prompt: &str,
    approval: ToolApprovalMode,
) -> (SessionHub, Arc<Notify>, Arc<MemNoticeStore>) {
    let (hub, opener, notices) = hub_full(system_prompt, approval);
    let gate = opener.provider.gate.clone();
    (hub, gate, notices)
}

fn hub_full(
    system_prompt: &str,
    approval: ToolApprovalMode,
) -> (SessionHub, Arc<TestOpener>, Arc<MemNoticeStore>) {
    let opener = Arc::new(TestOpener::new(system_prompt, approval));
    let notices = Arc::new(MemNoticeStore::default());
    (
        SessionHub::new(opener.clone(), notices.clone(), RelayConfig::default()),
        opener,
        notices,
    )
}

#[derive(Default)]
struct BlockingNoticeStore {
    inner: MemNoticeStore,
    save_started: Notify,
    allow_save: Notify,
}

impl NoticeStore for BlockingNoticeStore {
    fn load<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<TaskNotice>, String>> + Send + 'a>> {
        self.inner.load(key)
    }

    fn save<'a>(
        &'a self,
        key: &'a SessionKey,
        pending: &'a [TaskNotice],
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.save_started.notify_one();
            self.allow_save.notified().await;
            self.inner.put(key.clone(), pending.to_vec());
            Ok(())
        })
    }
}

fn key() -> SessionKey {
    ("ws".to_string(), "s1".to_string())
}

/// Await the next `RelayEvent` matching `pred`, skipping others.
async fn next_matching(
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

fn is_settling_llm_end(event: &RelayEvent) -> bool {
    matches!(
        event,
        RelayEvent::Event(e)
            if matches!(&**e, WireEvent::LlmEnd { message, .. } if message.tool_calls.is_empty())
    )
}

async fn wait_released(hub: &SessionHub) {
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

#[tokio::test(flavor = "multi_thread")]
async fn task_settles_then_reattach_shows_folded_history() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    assert!(attach1.snapshot.messages.is_empty());
    assert!(!attach1.snapshot.turn_running);

    let mut events1 = attach1.events;
    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "hello".into(),
                images: vec![],
            }
        )
        .await,
        CommandOutcome::Ok
    ));
    next_matching(&mut events1, is_settling_llm_end).await;

    // A second client takes over: folded history, no replay, first client
    // sees the eviction.
    let attach2 = hub
        .attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("attach2");
    assert!(!attach2.snapshot.turn_running);
    assert_eq!(attach2.snapshot.messages.len(), 2);
    assert!(matches!(&attach2.snapshot.messages[0], Message::User(_)));
    assert!(matches!(&attach2.snapshot.messages[1], Message::Assistant(a) if a.content == "done"));
    next_matching(&mut events1, |e| matches!(e, RelayEvent::Evicted)).await;

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn midturn_attach_replays_chunks_and_evicts_previous() {
    let (hub, gate) = hub_with("hold", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events1 = attach1.events;
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "go".into(),
            images: vec![],
        },
    )
    .await;
    // Wait until the partial chunk streamed to client 1: the turn is now
    // mid-flight.
    next_matching(&mut events1, |e| {
            matches!(e, RelayEvent::Event(ev) if matches!(&**ev, WireEvent::LlmContentChunk { .. }))
        })
        .await;

    let attach2 = hub
        .attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("attach2");
    // Mid-turn snapshot: the user prompt is visible, the turn is running,
    // and the chunk streamed so far is replayed.
    assert!(attach2.snapshot.turn_running);
    assert!(matches!(
        attach2.snapshot.messages.last(),
        Some(Message::User(_))
    ));
    let mut events2 = attach2.events;
    next_matching(&mut events2, |e| {
            matches!(
                e,
                RelayEvent::Event(ev)
                    if matches!(&**ev, WireEvent::LlmContentChunk { content, .. } if content == "partial")
            )
        })
        .await;
    next_matching(&mut events1, |e| matches!(e, RelayEvent::Evicted)).await;

    // A stale command from the evicted client is rejected.
    assert!(matches!(
        hub.command(key(), 1, SessionCommand::Abort).await,
        CommandOutcome::Ignored
    ));

    // Release the LLM stream; client 2 sees the turn finish live.
    gate.notify_one();
    next_matching(&mut events2, is_settling_llm_end).await;

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn detach_idle_releases_and_reattach_reopens_from_persisted_state() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events1 = attach1.events;
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "hello".into(),
            images: vec![],
        },
    )
    .await;
    next_matching(&mut events1, is_settling_llm_end).await;

    hub.detach(key(), 1).await;
    wait_released(&hub).await;

    // Reopen: history comes back from the persisted checkpoint.
    let attach2 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("re-attach");
    assert_eq!(attach2.snapshot.messages.len(), 2);
    assert!(!attach2.snapshot.turn_running);

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_all_waits_for_an_in_progress_release() {
    let opener = Arc::new(TestOpener::new("reply", ToolApprovalMode::Auto));
    let notices = Arc::new(BlockingNoticeStore::default());
    let hub = Arc::new(SessionHub::new(
        opener,
        notices.clone(),
        RelayConfig::default(),
    ));
    hub.attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");

    let entry = hub.get_entry(&key()).expect("entry exists");
    let mut guard = entry.inner.clone().lock_owned().await;
    let release = SessionHub::begin_release(
        &hub.entries,
        &entry,
        &mut guard,
        notices.clone(),
        Shutdown::graceful_unbounded(),
        false,
    );
    drop(guard);
    let release = tokio::spawn(release);
    timeout(Duration::from_secs(5), notices.save_started.notified())
        .await
        .expect("release did not reach notice persistence");

    let mut shutdown = {
        let hub = hub.clone();
        tokio::spawn(async move { hub.shutdown_all().await })
    };
    assert!(
        timeout(Duration::from_millis(50), &mut shutdown)
            .await
            .is_err(),
        "global shutdown returned before the active release finished"
    );

    notices.allow_save.notify_one();
    timeout(Duration::from_secs(5), async {
        release.await.expect("release task panicked");
        shutdown.await.expect("shutdown task panicked");
    })
    .await
    .expect("release barrier did not complete");
    assert!(hub.get_entry(&key()).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_during_turn_keeps_session_until_settle() {
    let (hub, gate) = hub_with("hold", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events1 = attach1.events;
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "go".into(),
            images: vec![],
        },
    )
    .await;
    next_matching(&mut events1, |e| {
            matches!(e, RelayEvent::Event(ev) if matches!(&**ev, WireEvent::LlmContentChunk { .. }))
        })
        .await;

    // Client vanishes mid-turn: the entry must survive (turn running).
    hub.detach_all(1).await;
    assert!(hub.get_entry(&key()).is_some());

    // The turn settles with nobody attached → the entry is released, with
    // the full history checkpointed.
    gate.notify_one();
    wait_released(&hub).await;

    let attach2 = hub
        .attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("re-attach");
    assert_eq!(attach2.snapshot.messages.len(), 2);
    assert!(matches!(&attach2.snapshot.messages[1], Message::Assistant(a) if a.content == "final"));

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn burst_of_chunks_survives_replay_and_fold() {
    let (hub, _) = hub_with("burst", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events1 = attach1.events;
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "burst".into(),
            images: vec![],
        },
    )
    .await;
    // 200 chunks stay within the broadcast channel's capacity (256), so
    // the burst is deterministically lossless; the pump must keep the
    // receiver drained and the turn settles normally.
    next_matching(&mut events1, is_settling_llm_end).await;

    let attach2 = hub
        .attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("attach2");
    assert_eq!(attach2.snapshot.messages.len(), 2);
    assert!(matches!(
        &attach2.snapshot.messages[1],
        Message::Assistant(a) if a.content == "burst done"
    ));

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runaway_tool_calls_force_resync_instead_of_unbounded_log() {
    let (hub, _) = hub_with("runaway", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events1 = attach1.events;
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "go".into(),
            images: vec![],
        },
    )
    .await;

    // The log crosses the configured message-tier cap long before the
    // fan-out turn could ever settle; the client is told to resync rather
    // than the hub buffering all of it in memory.
    next_matching(&mut events1, |e| matches!(e, RelayEvent::Closed)).await;
    wait_released(&hub).await;

    // Reopening reads the checkpoint the runtime saved once its (now
    // exit-barriered) tool execution batch finished.
    let attach2 = hub
        .attach(key(), 2, "prov".into(), None, false)
        .await
        .expect("re-attach");
    assert!(!attach2.snapshot.turn_running);
}

#[tokio::test(flavor = "multi_thread")]
async fn suspended_approval_survives_release_and_promotes_on_resume() {
    let (hub, _) = hub_with("approval", ToolApprovalMode::Manual);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events1 = attach1.events;
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "needs approval".into(),
            images: vec![],
        },
    )
    .await;
    let suspended = next_matching(
        &mut events1,
        |e| matches!(e, RelayEvent::Event(ev) if matches!(&**ev, WireEvent::Suspended { .. })),
    )
    .await;
    let RelayEvent::Event(event) = suspended else {
        unreachable!()
    };
    let WireEvent::Suspended { approval, .. } = *event else {
        unreachable!()
    };

    // Walk away: the suspended (settled) session is released.
    hub.detach(key(), 1).await;
    wait_released(&hub).await;

    // Reopen: the checkpointed approval gates the open (Pending entry).
    let attach2 = hub
        .attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("re-attach");
    assert_eq!(attach2.snapshot.pending_approvals.len(), 1);
    assert!(!attach2.snapshot.turn_running);
    let mut events2 = attach2.events;

    // Approving promotes the entry to live and the turn completes on the
    // stream registered at attach time.
    let outcome = hub
        .command(
            key(),
            2,
            SessionCommand::Resume {
                agent_name: approval.agent_name.clone(),
                thread_id: approval.thread_id.clone(),
                decision: ResumeDecision {
                    resolutions: vec![(approval.calls[0].id.clone(), ToolCallResolution::Execute)],
                },
            },
        )
        .await;
    assert!(matches!(outcome, CommandOutcome::Ok));
    next_matching(&mut events2, |e| {
        matches!(
            e,
            RelayEvent::Event(ev)
                if matches!(&**ev, WireEvent::LlmEnd { message, .. }
                    if message.content == "approved-done")
        )
    })
    .await;

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn new_task_clears_superseded_pending_approvals() {
    // Suspend for approval, then send a fresh task instead of resuming:
    // the driver discards the pending calls, so a later attach must not
    // advertise the stale approval.
    let (hub, _) = hub_with("approval", ToolApprovalMode::Manual);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events1 = attach1.events;
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "needs approval".into(),
            images: vec![],
        },
    )
    .await;
    next_matching(
        &mut events1,
        |e| matches!(e, RelayEvent::Event(ev) if matches!(&**ev, WireEvent::Suspended { .. })),
    )
    .await;

    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "never mind, do this instead".into(),
            images: vec![],
        },
    )
    .await;
    next_matching(&mut events1, is_settling_llm_end).await;

    let attach2 = hub
        .attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("attach2");
    assert!(attach2.snapshot.pending_approvals.is_empty());
    // The discarded call is folded as an aborted tool message, before the
    // superseding user prompt.
    assert!(attach2.snapshot.messages.iter().any(|m| matches!(
        m,
        Message::Tool(t) if matches!(t.outcome, coda_core::llm::ToolCallOutcome::Aborted)
    )));

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_evicts_attached_client_and_removes_entry() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events1 = attach1.events;

    assert!(hub.delete(key(), 1).await);
    next_matching(&mut events1, |e| matches!(e, RelayEvent::Evicted)).await;
    assert!(hub.get_entry(&key()).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_without_takeover_is_refused_while_held() {
    // Opening a session someone else is driving must not evict them
    // unless the caller explicitly asked for a takeover.
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events1 = attach1.events;

    assert!(matches!(
        hub.attach(key(), 2, "prov".into(), None, false).await,
        Err(AttachError::Busy)
    ));
    // The holder is untouched: no eviction was delivered.
    assert!(matches!(
        hub.command(key(), 1, SessionCommand::Abort).await,
        CommandOutcome::Ok
    ));

    // An explicit takeover still works and evicts the holder.
    hub.attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("takeover");
    next_matching(&mut events1, |e| matches!(e, RelayEvent::Evicted)).await;

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_from_stale_connection_is_rejected() {
    // Latest-wins covers destruction too: after being evicted, the old
    // connection must not be able to delete the session the new client is
    // driving.
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let _attach2 = hub
        .attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("attach2 evicts conn 1");

    assert!(!hub.delete(key(), 1).await);
    assert!(hub.get_entry(&key()).is_some());

    // The attached client itself may delete.
    assert!(hub.delete(key(), 2).await);
    assert!(hub.get_entry(&key()).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_resume_does_not_stick_turn_running() {
    // State is written only after the session accepted the command: a
    // failed resume must not flip `turn_running`, otherwise the entry
    // could never be released.
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Resume {
                agent_name: "ghost".into(),
                thread_id: "t-ghost".into(),
                decision: ResumeDecision {
                    resolutions: vec![],
                },
            },
        )
        .await,
        CommandOutcome::Ignored
    ));
    {
        let entry = hub.get_entry(&key()).expect("entry");
        let guard = entry.inner.clone().lock_owned().await;
        let EntryPhase::Live(live) = &guard.phase else {
            panic!("expected live entry");
        };
        assert!(!live.turn_running);
        assert!(live.unsettled_user_messages.is_empty());
    }

    // With no stuck flag, walking away releases the entry.
    hub.detach(key(), 1).await;
    wait_released(&hub).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lagged_stream_drains_session_and_closes_client() {
    // A lagged event stream means the in-memory view has a gap; the hub
    // must drain the session behind a checkpoint barrier and end the
    // client stream with `Closed` so it re-attaches from the persisted
    // state. Injected via a parallel forwarder — the real pump makes lag
    // (deliberately) hard to reproduce.
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events1 = attach1.events;

    let entry = hub.get_entry(&key()).expect("entry");
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run_forwarder(
        hub.entries.clone(),
        entry,
        hub.notices.clone(),
        rx,
        "coda".into(),
        0,
    ));
    tx.send(SessionStreamItem::Lagged(42)).expect("inject lag");

    next_matching(&mut events1, |e| matches!(e, RelayEvent::Closed)).await;
    wait_released(&hub).await;

    // Reopening reads the authoritative persisted checkpoint.
    let attach2 = hub
        .attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("re-attach");
    assert!(!attach2.snapshot.turn_running);
}

// --- background tasks: hub lifecycle --------------------------------------

fn task_meta(command: &str) -> TaskMeta {
    TaskMeta {
        command: command.into(),
        description: "lifecycle test task".into(),
        agent_name: "coda".into(),
    }
}

/// Spawn a fake task on the entry's registry that completes when `gate` fires
/// (or reports itself killed on cancellation).
fn completed_keys(id: &coda_tools::TaskId) -> Vec<coda_core::llm::TaskNoticeKey> {
    vec![coda_core::llm::TaskNoticeKey::Completed {
        task_id: id.as_str().to_owned(),
    }]
}

async fn spawn_gated_task(hub: &SessionHub, gate: Arc<Notify>) -> coda_tools::TaskId {
    let entry = hub.get_entry(&key()).expect("entry");
    entry
        .background()
        .spawn_with(task_meta("fake-long-command"), move |ctx| async move {
            let cancel = ctx.cancelled();
            tokio::select! {
                _ = gate.notified() => TaskExit::Exited { code: Some(0) },
                _ = cancel.cancelled() => TaskExit::Killed,
            }
        })
        .await
        .expect("spawn fake task")
}

fn running_tasks(entry: &Arc<SessionEntry>) -> usize {
    entry
        .background()
        .summaries()
        .borrow()
        .iter()
        .filter(|summary| summary.status.is_running())
        .count()
}

/// Roadmap ①+②: a running background task pins a disconnected idle entry;
/// when the last task finishes, the idle watcher releases the entry, the
/// notice lands in the NoticeStore, and the next attach restores it.
#[tokio::test(flavor = "multi_thread")]
async fn background_task_pins_disconnected_entry_then_release_persists_notice() {
    let (hub, _gate, notices) = hub_with_notices("reply", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let task_gate = Arc::new(Notify::new());
    let id = spawn_gated_task(&hub, task_gate.clone()).await;

    hub.detach(key(), 1).await;
    // A wrong release is spawned asynchronously; give it room to happen.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        hub.get_entry(&key()).is_some(),
        "a running background task must pin the detached entry"
    );

    task_gate.notify_one();
    wait_released(&hub).await;

    let persisted = notices.get(&key());
    assert_eq!(
        persisted.len(),
        1,
        "release persists the undelivered notice"
    );
    assert_eq!(persisted[0].keys(), completed_keys(&id));

    // Reopen: entry initialization restores the pending notice.
    let _attach2 = hub
        .attach(key(), 2, "prov".into(), None, false)
        .await
        .expect("re-attach");
    let entry = hub.get_entry(&key()).expect("entry");
    let restored = entry.background().take_notices().await;
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].keys(), completed_keys(&id));
}

/// Roadmap ③: a model switch swaps the session but not the entry-owned
/// registry — tasks keep running and their notices stay collectable.
#[tokio::test(flavor = "multi_thread")]
async fn model_switch_preserves_background_tasks_and_notices() {
    let (hub, _gate, _notices) = hub_with_notices("reply", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let entry_before = hub.get_entry(&key()).expect("entry");
    let task_gate = Arc::new(Notify::new());
    let id = spawn_gated_task(&hub, task_gate.clone()).await;

    let outcome = hub
        .command(
            key(),
            1,
            SessionCommand::SetModel {
                provider_id: "prov2".into(),
                reasoning_effort: None,
            },
        )
        .await;
    assert!(matches!(outcome, CommandOutcome::ModelChanged { .. }));

    let entry_after = hub.get_entry(&key()).expect("entry survives the swap");
    assert!(Arc::ptr_eq(
        entry_before.background(),
        entry_after.background()
    ));
    // The old session's asynchronous shutdown must leave the (external)
    // registry untouched; give it room to misbehave.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        running_tasks(&entry_after),
        1,
        "task survives the model switch"
    );

    task_gate.notify_one();
    let collected = timeout(Duration::from_secs(5), async {
        loop {
            let notices = entry_after.background().take_notices().await;
            if !notices.is_empty() {
                return notices;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("notice after completion");
    assert_eq!(collected[0].keys(), completed_keys(&id));

    hub.detach(key(), 1).await;
    wait_released(&hub).await;
}

/// Roadmap ④: multiple opens within one entry (here: the model-switch
/// replacement open) restore the persisted batch exactly once.
#[tokio::test(flavor = "multi_thread")]
async fn persisted_notices_restore_once_per_entry() {
    let (hub, _gate, notices) = hub_with_notices("reply", ToolApprovalMode::Auto);
    notices.put(
        key(),
        vec![TaskNotice::Task {
            id: BG_1.parse().unwrap(),
            command: "old command".into(),
            description: String::new(),
            status: TaskStatus::Exited {
                code: Some(0),
                at: jiff::Timestamp::now(),
            },
            output_tail: String::new(),
            stdout_overwritten: 0,
            stderr_overwritten: 0,
        }],
    );

    let _attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let outcome = hub
        .command(
            key(),
            1,
            SessionCommand::SetModel {
                provider_id: "prov2".into(),
                reasoning_effort: None,
            },
        )
        .await;
    assert!(matches!(outcome, CommandOutcome::ModelChanged { .. }));

    let entry = hub.get_entry(&key()).expect("entry");
    let restored = entry.background().take_notices().await;
    assert_eq!(
        restored.len(),
        1,
        "two opens in one entry must restore exactly once"
    );

    hub.detach(key(), 1).await;
    wait_released(&hub).await;
}

/// Roadmap ⑥+⑦ (Owned): a session-owned registry is torn down only once the
/// runtime confirmedly exited — a graceful timeout that returns `false`
/// leaves tasks running and the registry usable; the follow-up abort
/// completes the teardown.
#[tokio::test(flavor = "multi_thread")]
async fn owned_registry_teardown_follows_runtime_exit_confirmation() {
    let opener = TestOpener::new("hold", ToolApprovalMode::Auto);
    let session = Session::builder()
        .storage(opener.storage.clone())
        .team(&opener.team, ".")
        .run_config(RunConfig {
            default_model: ModelProfile {
                provider: opener.provider.clone(),
                model: "fake".into(),
                label: "fake".into(),
                temperature: None,
                max_completion_tokens: None,
                reasoning_effort: None,
            },
            agent_models: HashMap::new(),
            tool_approval: ToolApprovalMode::Auto,
            approval_timeout: None,
        })
        .open()
        .await
        .expect("open standalone session");

    // Hang a turn on the provider gate so the graceful shutdown times out.
    // Wait for the turn's first event: `send` only enqueues, and a shutdown
    // racing the dequeue would let the agent exit before the turn starts.
    session.send("hi", vec![]).await.expect("send");
    timeout(Duration::from_secs(5), async {
        loop {
            match session.recv().await {
                Some(coda_agent::SessionStreamItem::Event(event))
                    if matches!(
                        event.kind,
                        coda_agent::AgentEvent::LLMStart(_)
                            | coda_agent::AgentEvent::LLMContentChunk(_)
                    ) =>
                {
                    return;
                }
                Some(_) => {}
                None => panic!("session stream ended before the turn started"),
            }
        }
    })
    .await
    .expect("turn did not start");

    let task_gate = Arc::new(Notify::new());
    let g = task_gate.clone();
    session
        .background()
        .spawn_with(task_meta("owned"), move |ctx| async move {
            let cancel = ctx.cancelled();
            tokio::select! {
                _ = g.notified() => TaskExit::Exited { code: Some(0) },
                _ = cancel.cancelled() => TaskExit::Killed,
            }
        })
        .await
        .expect("spawn");

    let exited = session
        .shutdown(Shutdown::graceful(Duration::from_millis(100)))
        .await;
    assert!(!exited, "the held turn must time the graceful shutdown out");
    let running = session
        .background()
        .summaries()
        .borrow()
        .iter()
        .filter(|s| s.status.is_running())
        .count();
    assert_eq!(
        running, 1,
        "an unconfirmed exit must not tear down the owned registry"
    );

    // Unblock the held turn before the final shutdown: the driver consumed
    // the first shutdown's Exit and is awaiting the turn's completion, where
    // a later Abort cannot preempt it (pre-existing runtime behavior — see
    // the TODO in `AgentRuntime::wait_for_exit`).
    opener.provider.gate.notify_one();
    let exited = session.shutdown(Shutdown::abort()).await;
    assert!(exited);
    let running = session
        .background()
        .summaries()
        .borrow()
        .iter()
        .filter(|s| s.status.is_running())
        .count();
    assert_eq!(running, 0, "owned registry torn down after confirmed exit");
    let err = session
        .background()
        .spawn_with(task_meta("late"), |_ctx| async {
            TaskExit::Exited { code: None }
        })
        .await;
    assert!(err.is_err(), "owned registry is closed after teardown");
}

/// Review fix: a failed approvals-gated reopen must go through the release
/// path — a bare map removal would leave the entry's idle watcher (and the
/// client's relay stream) parked forever.
#[tokio::test(flavor = "multi_thread")]
async fn failed_gated_reopen_releases_the_entry_and_ends_the_stream() {
    let (hub, opener, _notices) = hub_full("approval", ToolApprovalMode::Manual);
    let attach1 = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events1 = attach1.events;
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "needs approval".into(),
            images: vec![],
        },
    )
    .await;
    let suspended = next_matching(
        &mut events1,
        |e| matches!(e, RelayEvent::Event(ev) if matches!(&**ev, WireEvent::Suspended { .. })),
    )
    .await;
    let RelayEvent::Event(event) = suspended else {
        unreachable!()
    };
    let WireEvent::Suspended { approval, .. } = *event else {
        unreachable!()
    };

    hub.detach(key(), 1).await;
    wait_released(&hub).await;

    // Reopen into the approvals-gated Pending phase, then make the
    // promotion open fail with a non-approval error.
    let attach2 = hub
        .attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("re-attach");
    let mut events2 = attach2.events;
    opener
        .fail_next_open
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let outcome = hub
        .command(
            key(),
            2,
            SessionCommand::Resume {
                agent_name: approval.agent_name.clone(),
                thread_id: approval.thread_id.clone(),
                decision: ResumeDecision {
                    resolutions: vec![(approval.calls[0].id.clone(), ToolCallResolution::Execute)],
                },
            },
        )
        .await;
    assert!(matches!(outcome, CommandOutcome::OpenFailed(_)));

    // The entry is gone (idle watcher retired via the registry shutdown) and
    // the client's stream terminates instead of dangling.
    wait_released(&hub).await;
    timeout(Duration::from_secs(5), async {
        while let Some(_event) = events2.next().await {}
    })
    .await
    .expect("relay stream must end after the failed gated reopen");
}

/// Attach yields the current background-task overview immediately; every
/// registry change (spawn, terminal commit) pushes a fresh full overview to
/// the attached client.
#[tokio::test]
async fn attach_delivers_background_overview_and_pushes_changes() {
    let (hub, _gate) = hub_with("reply", ToolApprovalMode::Auto);
    let attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events = attach.events;

    // Head of the stream: the (empty) overview as of attach.
    let RelayEvent::BackgroundTasks(tasks) =
        next_matching(&mut events, |e| matches!(e, RelayEvent::BackgroundTasks(_))).await
    else {
        unreachable!()
    };
    assert!(tasks.is_empty());

    let task_gate = Arc::new(Notify::new());
    let id = spawn_gated_task(&hub, task_gate.clone()).await;
    let RelayEvent::BackgroundTasks(tasks) = next_matching(
        &mut events,
        |e| matches!(e, RelayEvent::BackgroundTasks(tasks) if !tasks.is_empty()),
    )
    .await
    else {
        unreachable!()
    };
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, id.as_str());
    assert!(tasks[0].status.is_running());

    task_gate.notify_one();
    let RelayEvent::BackgroundTasks(tasks) = next_matching(&mut events, |e| {
        matches!(
            e,
            RelayEvent::BackgroundTasks(tasks)
                if !tasks.is_empty() && tasks.iter().all(|s| !s.status.is_running())
        )
    })
    .await
    else {
        unreachable!()
    };
    assert!(matches!(
        tasks[0].status,
        TaskStatus::Exited { code: Some(0), .. }
    ));
}
