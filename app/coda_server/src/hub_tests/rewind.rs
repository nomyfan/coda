//! `Rewind` command: truncating a turn and starting its replacement as one
//! step, including the race against a sub-agent's in-flight checkpoint write.

use super::super::*;
use super::fixtures::*;
use coda_agent::ToolApprovalMode;
use coda_agent::runtime::SessionStorage;
use tokio::time::Duration;

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

/// A rewind cannot race a sub-agent's checkpoint write, because by the time it
/// gets a chance there is no write left in flight. A sub-agent hands its reply
/// to the caller only once its own checkpoint is durable, and the caller cannot
/// reach the end of the turn without that reply — so stalling the write by
/// 300ms stalls the whole turn, rather than letting the root settle ahead of the
/// database and leaving a late write to put discarded history back.
#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_cannot_race_a_sub_agents_checkpoint_write() {
    let opener = Arc::new(TestOpener::delegating(
        "reply",
        Some(Duration::from_millis(300)),
    ));
    let storage = opener.storage.clone();
    let (hub, mut events, first_turn) = session_with_one_turn(opener).await;

    // The root turn has settled and the hub calls the session idle, so the
    // stalled write must already have landed.
    assert!(
        !stored_messages(&storage, explore_thread().as_ref())
            .await
            .is_empty(),
        "the root turn settled while the sub-agent's checkpoint was still unwritten"
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

    assert!(
        stored_messages(&storage, explore_thread().as_ref())
            .await
            .is_empty(),
        "the rewind left the sub-agent's discarded history behind"
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

/// The same guarantee under a supersede, which is where it used to break.
///
/// Sending the next task while `explore` is mid-write once ended the first turn
/// on the spot: the root wrote the call off as interrupted and settled a fresh
/// turn without ever waiting, leaving a write in flight for a rewind to race.
/// Now the superseded turn winds up properly — it waits for the real reply, and
/// the sub-agent sends that only once its own checkpoint is durable — so by the
/// time anything settles there is nothing left in flight.
fn root_answered(event: &RelayEvent) -> bool {
    matches!(event, RelayEvent::Event(e)
        if matches!(&**e, WireEvent::LlmEnd { agent_name, message, .. }
            if agent_name == "coda" && message.tool_calls.is_empty()))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_superseded_turn_leaves_no_write_in_flight() {
    let opener = Arc::new(TestOpener::delegating(
        "reply",
        Some(Duration::from_millis(300)),
    ));
    let storage = opener.storage.clone();
    let hub = SessionHub::new(opener, RelayConfig::default());
    let attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events = attach.events;

    let CommandOutcome::TaskAccepted {
        message_id: first_turn,
    } = hub
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

    // `explore` has started, so its stalled checkpoint write is what stands
    // between it and answering the root.
    next_matching(&mut events, |event| {
        matches!(event, RelayEvent::Event(e)
            if matches!(&**e, WireEvent::LlmStart { agent_name, .. } if agent_name == "explore"))
    })
    .await;
    assert!(
        stored_messages(&storage, explore_thread().as_ref())
            .await
            .is_empty(),
        "the sub-agent's write must still be in flight for this test to mean anything"
    );

    // The next task supersedes that turn.
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "instead".into(),
            images: vec![],
        },
    )
    .await;
    // The root's own answer, not the shared helper: winding the superseded turn
    // up means `explore` replies for real first, and its reply is a settling
    // `LlmEnd` too — waiting on that one would run ahead of the turn.
    next_matching(&mut events, root_answered).await;
    wait_idle(&hub).await;

    assert!(
        !stored_messages(&storage, explore_thread().as_ref())
            .await
            .is_empty(),
        "a turn settled while the sub-agent it superseded was still writing"
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
    next_matching(&mut events, root_answered).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        stored_messages(&storage, explore_thread().as_ref())
            .await
            .is_empty(),
        "the rewind left the superseded sub-agent's discarded history behind"
    );
}
