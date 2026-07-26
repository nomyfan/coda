use super::*;
use coda_agent::persist::StoredRuntimeSnapshot;
use coda_agent::runtime::{MemoryStorage, SessionStorage};
use coda_agent::{
    AgentSpec, AgentTeam, ModelProfile, RunConfig, SubAgentMode, ThreadId, ToolApprovalMode,
    ToolCallResolution,
};
use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, StreamError, ToolCall,
    ToolMessage, ToolOutput,
};
use coda_tools::ReadTodosToolSpec;
use futures::{Stream, StreamExt, stream};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};

// --- pure helpers -----------------------------------------------------

fn assistant(content: &str) -> AssistantMessage {
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
    Message::User(UserMessage::text(MessageId::new(), text.to_string()))
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
            // One call out to the `explore` sub-agent, then an answer. The
            // replacement turn a rewind starts says "different" and is answered
            // straight away, so the sub-agent's thread is only ever written by
            // the turn being discarded — which is what lets a test read that
            // thread afterwards and know whose messages it is looking at.
            "delegate" => {
                let replacement = request.messages.iter().any(
                    |m| matches!(m, Message::User(user) if user.first_text() == Some("different")),
                );
                let answered = request
                    .messages
                    .iter()
                    .any(|m| matches!(m, Message::Tool(_)));
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

/// `MemoryStorage` with a deliberate stall on one thread's checkpoint writes.
///
/// A sub-agent replies to its caller *before* saving its own checkpoint, so a
/// root turn can settle while a sub-agent's write is still on its way. That
/// window is real but short; widening it on purpose is what makes it testable
/// instead of something a test would only hit by luck.
#[derive(Clone, Default)]
struct SlowStorage {
    inner: MemoryStorage,
    stall: Option<(String, Duration)>,
}

impl SessionStorage for SlowStorage {
    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: coda_agent::persist::StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
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

struct TestOpener {
    storage: SlowStorage,
    provider: TestProvider,
    team: AgentTeam,
    approval: ToolApprovalMode,
    fail_effort_update: bool,
    /// Fail the rebuild a rewind performs after its truncation has committed.
    fail_open_after_rewind: bool,
    rewound: Arc<std::sync::atomic::AtomicBool>,
}

impl TestOpener {
    fn new(system_prompt: &str, approval: ToolApprovalMode) -> Self {
        let tools: Vec<Box<dyn coda_tools::ToolSpec>> =
            if matches!(system_prompt, "approval" | "runaway") {
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
    fn delegating(explore_prompt: &str, stall: Option<Duration>) -> Self {
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
            },
        )
    }

    fn with_team(team: AgentTeam, approval: ToolApprovalMode, storage: SlowStorage) -> Self {
        Self {
            storage,
            provider: TestProvider {
                gate: Arc::new(Notify::new()),
            },
            team,
            approval,
            fail_effort_update: false,
            fail_open_after_rewind: false,
            rewound: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

/// The thread the `explore` sub-agent runs in: stateful, so it is derived once
/// from the root thread (whose id is the session id) and stays put.
fn explore_thread() -> ThreadId {
    ThreadId::from_uuid5(&ThreadId::from(key().1), "explore")
}

impl SessionOpener for TestOpener {
    fn open<'a>(
        &'a self,
        key: &'a SessionKey,
        _provider_id: &'a str,
        _reasoning_effort: Option<String>,
        decisions: HashMap<String, ResumeDecision>,
    ) -> Pin<Box<dyn Future<Output = Result<Session, OpenError>> + Send + 'a>> {
        Box::pin(async move {
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
                    },
                    agent_models: HashMap::new(),
                    tool_approval: self.approval.clone(),
                    approval_timeout: None,
                })
                .session_id(key.1.clone())
                .resume_decisions(decisions)
                .open()
                .await?;
            Ok(session)
        })
    }

    fn load_messages<'a>(
        &'a self,
        _key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Vec<Message>> + Send + 'a>> {
        Box::pin(async { vec![] })
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
}

fn hub_with(system_prompt: &str, approval: ToolApprovalMode) -> (SessionHub, Arc<Notify>) {
    let opener = Arc::new(TestOpener::new(system_prompt, approval));
    let gate = opener.provider.gate.clone();
    (SessionHub::new(opener, RelayConfig::default()), gate)
}

fn hub_with_failing_metadata(system_prompt: &str) -> SessionHub {
    let mut opener = TestOpener::new(system_prompt, ToolApprovalMode::Auto);
    opener.fail_effort_update = true;
    SessionHub::new(Arc::new(opener), RelayConfig::default())
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

/// Wait until the forwarder has folded the settled turn.
///
/// A client is handed the settling event *before* the forwarder updates the
/// entry it came from, so a test that acts the moment that event arrives can
/// beat the hub to its own bookkeeping and see a session that is still marked
/// as running.
async fn wait_idle(hub: &SessionHub) {
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
        CommandOutcome::TaskAccepted { .. }
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

/// Every message reaches the relay's snapshot by a different route than it
/// reaches storage, and the two must agree on its id — otherwise one message
/// has two identities and anything naming a message across a reconnect (a
/// rewind target, a front-end key) only addresses half the system.
///
/// The routes differ per variant, which is why this asserts on the whole
/// sequence rather than one message: a user message is *built twice* (once in
/// the session, once here) and only agrees because the id is minted before
/// either copy; assistant and tool messages are built once and ride the event
/// pipeline here while the driver writes the same object to history.
///
/// The user message has a third consumer — the id returned to the client that
/// sent the task — so that one is checked against both copies too.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_and_checkpoint_agree_on_every_message_id() {
    // The "approval" script calls a tool and then answers, so one turn produces
    // all three persisted variants: user, assistant (with tool calls), tool,
    // assistant. `Auto` approval keeps it from suspending.
    let opener = Arc::new(TestOpener::new("approval", ToolApprovalMode::Auto));
    let storage = opener.storage.clone();
    let hub = SessionHub::new(opener, RelayConfig::default());

    let attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events = attach.events;
    let outcome = hub
        .command(
            key(),
            1,
            SessionCommand::Task {
                task: "go".into(),
                images: vec![],
            },
        )
        .await;
    let CommandOutcome::TaskAccepted { message_id: acked } = outcome else {
        panic!("a task against a live session is accepted");
    };
    next_matching(&mut events, is_settling_llm_end).await;

    // Read the snapshot the way a reconnecting client would.
    let snapshot = hub
        .attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("attach2")
        .snapshot;

    // Graceful shutdown drains the driver, which writes its checkpoint before
    // it observes the exit signal — so the persisted history is settled here.
    hub.shutdown_all().await;
    let persisted = storage
        .load_checkpoint(&key().1)
        .await
        .expect("load checkpoint")
        .expect("root thread checkpoint was written")
        .messages
        .into_iter()
        .map(|entry| entry.message)
        .collect::<Vec<_>>();

    assert_eq!(ids_by_role(&snapshot.messages), ids_by_role(&persisted));
    // Guard the assertion above against passing on two empty lists, and pin
    // that the turn really did exercise all three variants.
    assert_eq!(
        ids_by_role(&persisted)
            .iter()
            .map(|(role, _)| *role)
            .collect::<Vec<_>>(),
        vec!["user", "assistant", "tool", "assistant"]
    );
    // The id handed back to the client is the same one both copies carry.
    assert_eq!(ids_by_role(&persisted)[0], ("user", acked));
}

/// Each message's role and id, in order — what two copies of one history must
/// agree on.
fn ids_by_role(messages: &[Message]) -> Vec<(&'static str, MessageId)> {
    messages
        .iter()
        .map(|m| match m {
            Message::User(u) => ("user", u.message_id),
            Message::Assistant(a) => ("assistant", a.message_id),
            Message::Tool(t) => ("tool", t.message_id),
            // Built fresh for each request and never persisted, so it has no id
            // and cannot appear in either list.
            Message::System(_) => unreachable!("a system message reached persisted history"),
        })
        .collect()
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

#[tokio::test(flavor = "multi_thread")]
async fn set_model_to_current_selection_is_unchanged() {
    // Re-selecting the model already in effect is a benign no-op the dispatcher
    // reports as idempotent success (Decision 8).
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::SetModel {
                provider_id: "prov".into(),
                reasoning_effort: None,
            },
        )
        .await,
        CommandOutcome::Unchanged
    ));

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_effort_switch_returns_model_changed() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::SetModel {
                provider_id: "prov".into(),
                reasoning_effort: Some("high".into()),
            },
        )
        .await,
        CommandOutcome::ModelChanged { provider_id, reasoning_effort }
            if provider_id == "prov" && reasoning_effort.as_deref() == Some("high")
    ));

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_rejects_a_different_provider_or_model() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(key(), 1, "prov:model-a".into(), None, false)
        .await
        .expect("attach");

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::SetModel {
                provider_id: "prov:model-b".into(),
                reasoning_effort: None,
            },
        )
        .await,
        CommandOutcome::ModelLocked
    ));

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_effort_persistence_keeps_live_selection() {
    let hub = hub_with_failing_metadata("reply");
    let _attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::SetModel {
                provider_id: "prov".into(),
                reasoning_effort: Some("high".into()),
            },
        )
        .await,
        CommandOutcome::PersistenceFailed(ref error)
            if error == "injected metadata write failure"
    ));
    let refreshed = hub
        .attach(key(), 1, "prov".into(), Some("high".into()), false)
        .await
        .expect("refresh attach");
    assert_eq!(refreshed.snapshot.provider_id, "prov");
    assert_eq!(refreshed.snapshot.reasoning_effort, None);

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_while_turn_running_is_rejected() {
    // A live session can only be rebuilt while idle; a switch during an
    // in-flight turn is a soft reject (→ MODEL_SWITCH_WHILE_RUNNING), not a
    // silent `Ignored` that the dispatcher would misread as SESSION_NOT_LIVE.
    let (hub, gate) = hub_with("hold", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");

    // `handle_task` flips `turn_running` synchronously once the session accepts
    // the task, so the following `set_model` observes a running turn.
    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "hold on".into(),
                images: vec![],
            },
        )
        .await,
        CommandOutcome::TaskAccepted { .. }
    ));

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::SetModel {
                provider_id: "other".into(),
                reasoning_effort: None,
            },
        )
        .await,
        CommandOutcome::TurnRunning
    ));

    // Let the held turn settle so shutdown is prompt.
    gate.notify_one();
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_on_unattached_connection_is_ignored() {
    // The stale/not-attached guard in `command` returns `Ignored` *before*
    // dispatch; the request layer reads that as SESSION_NOT_LIVE.
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");

    // Connection 2 never attached: its command is refused at the guard.
    assert!(matches!(
        hub.command(
            key(),
            2,
            SessionCommand::SetModel {
                provider_id: "other".into(),
                reasoning_effort: None,
            },
        )
        .await,
        CommandOutcome::Ignored
    ));

    hub.shutdown_all().await;
}

// --- rewind ------------------------------------------------------------

/// Start a session and run one turn, returning the hub, the event stream, and
/// the id of the user message that turn began with — the thing a rewind names.
async fn session_with_one_turn(
    opener: Arc<TestOpener>,
) -> (SessionHub, BoxStream<'static, RelayEvent>, MessageId) {
    let hub = SessionHub::new(opener, RelayConfig::default());
    let attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events = attach.events;
    let CommandOutcome::TaskAccepted { message_id } = hub
        .command(
            key(),
            1,
            SessionCommand::Task {
                task: "go".into(),
                images: vec![],
            },
        )
        .await
    else {
        panic!("a task against a live session is accepted");
    };
    next_matching(&mut events, is_settling_llm_end).await;
    wait_idle(&hub).await;
    (hub, events, message_id)
}

async fn stored_messages(storage: &SlowStorage, thread_id: &str) -> Vec<Message> {
    storage
        .load_checkpoint(thread_id)
        .await
        .expect("load checkpoint")
        .map(|checkpoint| {
            checkpoint
                .messages
                .into_iter()
                .map(|entry| entry.message)
                .collect()
        })
        .unwrap_or_default()
}

/// The window this whole design is shaped around: a sub-agent replies to its
/// caller *before* saving its own checkpoint, so the root turn can settle — and
/// the hub can call itself idle — while that write is still in flight. Truncate
/// then, and the late write puts the discarded tail straight back, because
/// against a lowered message count it reads as ordinary growth.
///
/// Stopping the runtime first is the only barrier that rules this out. Drop the
/// `shutdown` from `handle_rewind` and this test fails.
#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_waits_out_a_sub_agent_that_replied_before_it_saved() {
    let opener = Arc::new(TestOpener::delegating(
        "reply",
        Some(Duration::from_millis(300)),
    ));
    let storage = opener.storage.clone();
    let (hub, mut events, first_turn) = session_with_one_turn(opener).await;

    // The root turn has settled, so the hub considers the session idle — but
    // `explore` is still inside its stalled checkpoint write.
    assert!(
        stored_messages(&storage, explore_thread().as_ref())
            .await
            .is_empty(),
        "the sub-agent's write must still be in flight for this test to mean anything"
    );

    let outcome = hub
        .command(
            key(),
            1,
            SessionCommand::Rewind {
                target: first_turn,
                task: "different".into(),
                images: vec![],
            },
        )
        .await;
    assert!(matches!(outcome, CommandOutcome::Rewound { .. }));
    next_matching(&mut events, is_settling_llm_end).await;

    // Outlast the stall before looking. Without the barrier the truncation runs
    // first and the stalled write lands *after* it — but the replacement turn
    // finishes in microseconds, so an assertion taken straight after it would
    // read the sub-agent's thread while the damaging write is still asleep and
    // see the empty history it expects for entirely the wrong reason.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        stored_messages(&storage, explore_thread().as_ref())
            .await
            .is_empty(),
        "the sub-agent's late checkpoint must not restore the turn that was discarded"
    );
}

/// The truncation and the turn that replaces it are one step, and the client is
/// told what survived so it does not have to work that out for itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_replaces_the_discarded_turn_and_reports_what_survived() {
    let opener = Arc::new(TestOpener::new("reply", ToolApprovalMode::Auto));
    let (hub, mut events, _first_turn) = session_with_one_turn(opener).await;

    // A second turn, so the rewind has something to keep as well as something
    // to discard.
    let CommandOutcome::TaskAccepted {
        message_id: second_turn,
    } = hub
        .command(
            key(),
            1,
            SessionCommand::Task {
                task: "and then this".into(),
                images: vec![],
            },
        )
        .await
    else {
        panic!("a task against a live session is accepted");
    };
    next_matching(&mut events, is_settling_llm_end).await;

    let outcome = hub
        .command(
            key(),
            1,
            SessionCommand::Rewind {
                target: second_turn,
                task: "different".into(),
                images: vec![],
            },
        )
        .await;
    let CommandOutcome::Rewound {
        message_id,
        messages,
    } = outcome
    else {
        panic!("expected the rewind to succeed");
    };
    assert_ne!(
        message_id, second_turn,
        "the edited message is a new message, not a rewrite of the discarded one"
    );
    assert_eq!(
        messages.len(),
        2,
        "only the first turn survives: its user message and the answer to it"
    );
    next_matching(&mut events, is_settling_llm_end).await;

    // What an attaching client sees is the surviving history plus the edited
    // message — the same thing the command reported.
    let snapshot = hub
        .attach(key(), 2, "prov".into(), None, true)
        .await
        .expect("re-attach")
        .snapshot;
    let texts: Vec<String> = snapshot
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user.first_text().unwrap_or_default().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["go".to_string(), "different".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_is_refused_while_a_turn_is_in_flight() {
    let (hub, gate) = hub_with("hold", ToolApprovalMode::Auto);
    let attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let CommandOutcome::TaskAccepted { message_id } = hub
        .command(
            key(),
            1,
            SessionCommand::Task {
                task: "go".into(),
                images: vec![],
            },
        )
        .await
    else {
        panic!("a task against a live session is accepted");
    };

    let outcome = hub
        .command(
            key(),
            1,
            SessionCommand::Rewind {
                target: message_id,
                task: "different".into(),
                images: vec![],
            },
        )
        .await;
    assert!(matches!(outcome, CommandOutcome::NotIdle));

    // The turn was never disturbed: releasing the gate still finishes it.
    gate.notify_one();
    let mut events = attach.events;
    next_matching(&mut events, is_settling_llm_end).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_is_refused_while_a_call_waits_on_a_human() {
    let (hub, _gate) = hub_with("approval", ToolApprovalMode::Manual);
    let attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events = attach.events;
    let CommandOutcome::TaskAccepted { message_id } = hub
        .command(
            key(),
            1,
            SessionCommand::Task {
                task: "go".into(),
                images: vec![],
            },
        )
        .await
    else {
        panic!("a task against a live session is accepted");
    };
    next_matching(&mut events, |event| {
        matches!(event, RelayEvent::Event(e) if matches!(&**e, WireEvent::Suspended { .. }))
    })
    .await;

    // The turn has settled — suspension settles it — so `turn_running` alone
    // would let this through. The pending approval is what must not.
    let outcome = hub
        .command(
            key(),
            1,
            SessionCommand::Rewind {
                target: message_id,
                task: "different".into(),
                images: vec![],
            },
        )
        .await;
    assert!(matches!(outcome, CommandOutcome::NotIdle));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_rewind_leaves_the_session_exactly_as_it_was() {
    let opener = Arc::new(TestOpener::new("reply", ToolApprovalMode::Auto));
    let (hub, _events, _) = session_with_one_turn(opener).await;

    let outcome = hub
        .command(
            key(),
            1,
            SessionCommand::Rewind {
                // An id that names nothing: the truncation never runs.
                target: MessageId::new(),
                task: "different".into(),
                images: vec![],
            },
        )
        .await;
    assert!(matches!(outcome, CommandOutcome::RewindTargetNotFound));

    // The entry still serves the history it had, and still takes work. A
    // re-attach hands back a fresh stream (the old one is retired with the
    // channel it was registered on), so carry on with that.
    let refreshed = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("still attached");
    let mut events = refreshed.events;
    assert_eq!(refreshed.snapshot.messages.len(), 2);
    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "carry on".into(),
                images: vec![],
            },
        )
        .await,
        CommandOutcome::TaskAccepted { .. }
    ));
    next_matching(&mut events, is_settling_llm_end).await;
}

/// Once the truncation has committed, the client's view is stale no matter what
/// goes wrong next. Both remaining failures therefore end the same way — the
/// runtime is dropped and the client is told to re-attach — rather than each
/// inventing its own way back. That is the route a crash would have forced
/// anyway, so it is the only recovery path there is.
#[tokio::test(flavor = "multi_thread")]
async fn a_rebuild_that_fails_after_the_truncation_sends_the_client_back_for_a_fresh_attach() {
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.fail_open_after_rewind = true;
    let (hub, mut events, first_turn) = session_with_one_turn(Arc::new(opener)).await;

    let outcome = hub
        .command(
            key(),
            1,
            SessionCommand::Rewind {
                target: first_turn,
                task: "different".into(),
                images: vec![],
            },
        )
        .await;
    assert!(matches!(outcome, CommandOutcome::OpenFailed(_)));
    assert!(matches!(
        next_matching(&mut events, |event| matches!(event, RelayEvent::Closed)).await,
        RelayEvent::Closed
    ));
    assert!(
        hub.get_entry(&key()).is_none(),
        "the slot must be free so the next attach reads the truncated state"
    );
}
