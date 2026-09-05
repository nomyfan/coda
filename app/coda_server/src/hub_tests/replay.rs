//! Attach/reattach/replay lifecycle over a live hub: folded history on
//! reconnect, mid-turn replay, and eviction on takeover. Deletion has its own
//! category — see `delete.rs`.

use super::super::*;
use super::fixtures::*;
use coda_agent::ToolApprovalMode;
use coda_agent::runtime::SessionStorage;
use tokio::sync::mpsc;

#[tokio::test(flavor = "multi_thread")]
async fn task_settles_then_reattach_shows_folded_history() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
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
        .attach(
            key(),
            2,
            "prov".into(),
            None,
            PermissionMode::default(),
            true,
        )
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
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
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
        .attach(
            key(),
            2,
            "prov".into(),
            None,
            PermissionMode::default(),
            true,
        )
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
        .map(|m| {
            let role = match m {
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::Tool(_) => "tool",
                Message::Compaction(_) => "compaction",
                Message::TaskNotice(_) => "task_notice",
            };
            (role, m.message_id())
        })
        .collect()
}

/// Mid-turn auto-compaction appends a `Compaction` message straight to the
/// driver's history, outside the hub's `compact` command — it only reaches
/// the live snapshot if it also travels the event pipeline. Without that, a
/// client that never disconnects would never see it.
#[tokio::test(flavor = "multi_thread")]
async fn auto_compaction_reaches_the_live_snapshot_without_a_reattach() {
    let mut opener = TestOpener::new("auto-compact", ToolApprovalMode::Auto);
    opener.auto_compact_threshold_tokens = 1_000;
    let storage = opener.storage.clone();
    let hub = SessionHub::new(Arc::new(opener), RelayConfig::default());

    let attach = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
        .await
        .expect("attach");
    let mut events = attach.events;

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "go".into(),
                images: vec![],
            }
        )
        .await,
        CommandOutcome::TaskAccepted { .. }
    ));
    next_matching(&mut events, is_settling_llm_end).await;

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "go".into(),
                images: vec![],
            }
        )
        .await,
        CommandOutcome::TaskAccepted { .. }
    ));
    next_matching(&mut events, is_settling_llm_end).await;

    // The live snapshot as an already-attached client would see it: no
    // reattach, no takeover, no reload from storage.
    let snapshot = with_live(&hub, |live| live.snapshot.clone()).await;

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

    assert!(
        ids_by_role(&persisted)
            .iter()
            .any(|(role, _)| *role == "compaction"),
        "the scripted second turn should have crossed the auto-compact \
         threshold: {:?}",
        ids_by_role(&persisted)
    );
    assert_eq!(ids_by_role(&snapshot), ids_by_role(&persisted));
}

#[tokio::test(flavor = "multi_thread")]
async fn midturn_attach_replays_chunks_and_evicts_previous() {
    let (hub, gate) = hub_with("hold", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
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
        .attach(
            key(),
            2,
            "prov".into(),
            None,
            PermissionMode::default(),
            true,
        )
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
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
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
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
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
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
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
        .attach(
            key(),
            2,
            "prov".into(),
            None,
            PermissionMode::default(),
            true,
        )
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
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
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
        .attach(
            key(),
            2,
            "prov".into(),
            None,
            PermissionMode::default(),
            true,
        )
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
async fn overflowing_tool_calls_refresh_snapshot_without_stopping_background_work() {
    let (hub, _) = hub_with("runaway", ToolApprovalMode::Auto);
    let attached = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
        .await
        .unwrap();
    let mut events = attached.events;
    let background = background_of(&hub).await;
    let id = background
        .spawn_with(task_meta("survives resync"), |ctx| async move {
            ctx.cancelled().cancelled().await;
            coda_process::TaskExit::Killed
        })
        .await
        .unwrap();
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "go".into(),
            images: vec![],
        },
    )
    .await;
    next_matching(&mut events, |event| {
        matches!(event, RelayEvent::Snapshot(_))
    })
    .await;
    next_matching(&mut events, is_settling_llm_end).await;
    assert!(
        background
            .read(&id)
            .await
            .unwrap()
            .unwrap()
            .status
            .is_running()
    );
    let entry = hub.get_entry(&key()).unwrap();
    let guard = entry.inner.lock().await;
    let EntryPhase::Live(live) = &guard.phase else {
        panic!("resync must retain runtime")
    };
    assert!(!live.log.message_tier_overflowed());
    drop(guard);
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_without_takeover_is_refused_while_held() {
    // Opening a session someone else is driving must not evict them
    // unless the caller explicitly asked for a takeover.
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let attach1 = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
        .await
        .expect("attach");
    let mut events1 = attach1.events;

    assert!(matches!(
        hub.attach(
            key(),
            2,
            "prov".into(),
            None,
            PermissionMode::default(),
            false
        )
        .await,
        Err(AttachError::Busy)
    ));
    // The holder is untouched: no eviction was delivered.
    assert!(matches!(
        hub.command(key(), 1, SessionCommand::Abort).await,
        CommandOutcome::Ok
    ));

    // An explicit takeover still works and evicts the holder.
    hub.attach(
        key(),
        2,
        "prov".into(),
        None,
        PermissionMode::default(),
        true,
    )
    .await
    .expect("takeover");
    next_matching(&mut events1, |e| matches!(e, RelayEvent::Evicted)).await;

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_resume_does_not_stick_turn_running() {
    // State is written only after the session accepted the command: a
    // failed resume must not flip `turn_running`, otherwise the entry
    // could never be released.
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach1 = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
        .await
        .expect("attach");

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Resume {
                allow_patterns: vec![],
                agent_name: "ghost".into(),
                thread_id: "t-ghost".into(),
                decision: ResumeDecision {
                    parent_message_id: MessageId::new(),
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
        assert!(live.unsettled_user_message.is_none());
    }

    // With no stuck flag, walking away releases the entry.
    hub.detach(key(), 1).await;
    wait_released(&hub).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lagged_stream_refreshes_snapshot_and_keeps_runtime_attached() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let attached = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
        .await
        .unwrap();
    let mut events = attached.events;
    let entry = hub.get_entry(&key()).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let forwarder = tokio::spawn(run_forwarder(
        hub.entries.clone(),
        entry.clone(),
        rx,
        "coda".into(),
        0,
        hub.opener.clone(),
        hub.status_tx.clone(),
    ));
    tx.send(SessionStreamItem::Lagged(42)).unwrap();
    next_matching(&mut events, |event| {
        matches!(event, RelayEvent::Snapshot(_))
    })
    .await;
    assert!(Arc::ptr_eq(&entry, &hub.get_entry(&key()).unwrap()));
    forwarder.abort();
    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "still works".into(),
                images: vec![]
            }
        )
        .await,
        CommandOutcome::TaskAccepted { .. }
    ));
    next_matching(&mut events, is_settling_llm_end).await;
    hub.shutdown_all().await;
}

/// A checkpoint the database refuses leaves the in-memory view describing a
/// turn nothing can back. The client is told why, then sent back to the
/// persisted state — the same route a lagged stream takes. What it finds there
/// is the turn's prompt without its answer, which is the truth: the answer was
/// never stored, so it never happened.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_checkpoint_reports_the_failure_then_resyncs() {
    let opener = Arc::new(TestOpener::new("reply", ToolApprovalMode::Auto));
    let storage = opener.storage.clone();
    let hub = SessionHub::new(opener, RelayConfig::default());
    let attach = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
        .await
        .expect("attach");
    let mut events = attach.events;

    // The opening write of the prompt goes through; the one that would make the
    // turn's answer durable does not.
    storage.fail_checkpoints_after(1).await;
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "go".into(),
            images: vec![],
        },
    )
    .await;

    next_matching(&mut events, |event| {
        matches!(event, RelayEvent::Event(e) if matches!(&**e, WireEvent::PersistFailed { .. }))
    })
    .await;
    next_matching(&mut events, |event| matches!(event, RelayEvent::Closed)).await;
    wait_released(&hub).await;

    let attach2 = hub
        .attach(
            key(),
            2,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
        .await
        .expect("re-attach");
    assert!(!attach2.snapshot.turn_running);
    assert!(
        matches!(
            attach2.snapshot.messages.as_slice(),
            [Message::User(user)] if user.first_text() == Some("go")
        ),
        "expected the prompt alone, got {:?}",
        attach2.snapshot.messages
    );
}
