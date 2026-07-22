use super::*;
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
use futures::{Stream, StreamExt, stream};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};

// --- pure helpers -----------------------------------------------------

fn assistant(content: &str) -> AssistantMessage {
    let now = jiff::Timestamp::now();
    AssistantMessage {
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
    fail_effort_update: bool,
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
            fail_effort_update: false,
        }
    }
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
        CommandOutcome::Ok
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
