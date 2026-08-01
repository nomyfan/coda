//! `fork`: idle/busy gating around the copy, and the race between the gate's
//! cleanup and a concurrent attach.

use super::super::*;
use super::fixtures::*;
use coda_agent::ToolApprovalMode;
use tokio::sync::Notify;
use tokio::time::Duration;

/// A task sent during a running turn queues rather than being refused, and a
/// settling turn only pops its own message. So `turn_running` going false does
/// not mean the session is idle — there may be a task the runtime has not
/// reached yet, and a fork landing there would copy a session that is about to
/// grow.
#[tokio::test]
async fn forking_is_refused_while_a_task_is_queued_behind_the_current_turn() {
    let (hub, opener) = hub_and_opener(TestOpener::new("hold", ToolApprovalMode::Auto));
    let _attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    for task in ["first", "second"] {
        let outcome = hub
            .command(
                key(),
                1,
                SessionCommand::Task {
                    task: task.into(),
                    images: vec![],
                },
            )
            .await;
        assert!(
            matches!(outcome, CommandOutcome::TaskAccepted { .. }),
            "a task sent during a running turn queues instead of being refused"
        );
    }
    assert_eq!(
        with_live(&hub, |live| live.unsettled_user_messages.len()).await,
        2,
        "both submissions are waiting to settle"
    );

    // The window the forwarder opens between one turn settling and the next
    // one's first event: the flag is already down, the queue is not empty.
    with_live(&hub, |live| live.turn_running = false).await;

    assert!(
        matches!(hub.fork(key(), None).await, ForkOutcome::NotIdle),
        "a queued task makes the session busy even with the flag down"
    );
    assert!(
        opener.forks.lock().unwrap().is_empty(),
        "a refused fork never reaches storage"
    );
}

/// Nothing live means the stored state is the whole truth, so there is no
/// in-memory length to check it against — and the entry the gate borrowed must
/// not be left behind.
#[tokio::test]
async fn forking_a_session_nobody_opened_leaves_no_entry_behind() {
    let (hub, opener) = hub_and_opener(TestOpener::new("reply", ToolApprovalMode::Auto));

    let outcome = hub.fork(key(), None).await;

    assert!(matches!(outcome, ForkOutcome::Forked(_)));
    assert_eq!(
        opener.forks.lock().unwrap().as_slice(),
        [(ForkCut::All, ForkSource::Cold)]
    );
    assert!(
        hub.get_entry(&key()).is_none(),
        "the borrowed entry is removed, not left as an empty shell"
    );
}

/// A full copy of a live session carries the length the client is looking at, so
/// storage can tell "everything" from "everything stored so far".
#[tokio::test]
async fn forking_a_live_session_carries_its_in_memory_length() {
    let (hub, opener) = hub_and_opener(TestOpener::new("reply", ToolApprovalMode::Auto));
    let attach = hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    let mut events = attach.events;
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
        matches!(event, RelayEvent::Event(event) if matches!(**event, WireEvent::LlmEnd { .. }))
    })
    .await;

    let outcome = hub.fork(key(), None).await;

    assert!(matches!(outcome, ForkOutcome::Forked(_)));
    assert_eq!(
        opener.forks.lock().unwrap().as_slice(),
        [(ForkCut::All, ForkSource::Live { root_messages: 2 })],
        "the settled user message and reply are what the client can see"
    );
    assert!(
        hub.get_entry(&key()).is_some(),
        "a session that was already live stays live"
    );
}

/// The gate's cleanup races attach: an attach can clone the borrowed entry out
/// of the map and then block on its mutex, so removing the entry without a
/// tombstone would let it open a runtime nothing can look up again.
#[tokio::test]
async fn an_attach_racing_the_gates_cleanup_gets_a_fresh_entry() {
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    let fork_gate = Arc::new(Notify::new());
    opener.fork_gate = Some(fork_gate.clone());
    let (hub, opener) = hub_and_opener(opener);

    let forking = tokio::spawn({
        let hub = hub.clone();
        async move { hub.fork(key(), None).await }
    });
    // Wait until the fork is inside storage, holding the entry it created.
    while opener.forks.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }

    let attaching = tokio::spawn({
        let hub = hub.clone();
        async move { hub.attach(key(), 1, "prov".into(), None, false).await }
    });
    // Let the attach reach the map and block on the mutex, so it is holding the
    // very entry the fork is about to drop.
    tokio::time::sleep(Duration::from_millis(50)).await;

    fork_gate.notify_waiters();
    assert!(matches!(
        forking.await.expect("fork task"),
        ForkOutcome::Forked(_)
    ));
    let attached = attaching
        .await
        .expect("attach task")
        .expect("the attach survives the entry it waited on being removed");
    drop(attached);

    // The proof it landed on a live slot the hub can find: a command routes.
    assert!(
        matches!(
            hub.command(
                key(),
                1,
                SessionCommand::Task {
                    task: "go".into(),
                    images: vec![],
                },
            )
            .await,
            CommandOutcome::TaskAccepted { .. }
        ),
        "the attach owns an entry that is still in the map"
    );
}

/// `ThreadBusy` means different things depending on who is asking. With a
/// runtime attached the session was just checked to be idle, so a thread parked
/// mid-turn in the database is a checkpoint still in flight — retry. With no
/// runtime, the stored state is all there is and it really is parked.
#[tokio::test]
async fn a_busy_thread_is_retryable_only_while_the_source_is_live() {
    let busy = ForkError::ThreadBusy {
        thread_id: "s1".into(),
    };

    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.fork_error = Some(busy.clone());
    let (cold_hub, _) = hub_and_opener(opener);
    assert!(
        matches!(cold_hub.fork(key(), None).await, ForkOutcome::Failed(_)),
        "nothing live: the stored state is the whole truth"
    );

    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.fork_error = Some(busy);
    let (live_hub, _) = hub_and_opener(opener);
    let _attach = live_hub
        .attach(key(), 1, "prov".into(), None, false)
        .await
        .expect("attach");
    assert!(
        matches!(live_hub.fork(key(), None).await, ForkOutcome::Retryable(_)),
        "live and idle: the database is only lagging"
    );
}

/// Storage's own idle check is the cold twin of the gate's, so it has to come
/// back to the client as the same refusal a live session would have produced.
#[tokio::test]
async fn a_cold_source_holding_queued_work_refuses_like_a_busy_one() {
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.fork_error = Some(ForkError::SourceNotIdle {
        thread_id: "s1".into(),
    });
    let (hub, _) = hub_and_opener(opener);

    assert!(matches!(hub.fork(key(), None).await, ForkOutcome::NotIdle));
}
