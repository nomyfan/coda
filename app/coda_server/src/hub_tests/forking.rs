//! `fork`: idle/busy gating around the copy, and the race between the gate's
//! cleanup and a concurrent attach.

use super::super::*;
use super::fixtures::*;
use coda_agent::ToolApprovalMode;
use tokio::sync::Notify;
use tokio::time::Duration;

#[tokio::test]
async fn a_second_task_is_rejected_while_the_first_keeps_fork_busy() {
    let (hub, opener) = hub_and_opener(TestOpener::new("hold", ToolApprovalMode::Auto));
    let _attach = hub
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
    let first = hub
        .command(
            key(),
            1,
            SessionCommand::Task {
                task: "first".into(),
                images: vec![],
            },
        )
        .await;
    assert!(matches!(first, CommandOutcome::TaskAccepted { .. }));
    let second = hub
        .command(
            key(),
            1,
            SessionCommand::Task {
                task: "second".into(),
                images: vec![],
            },
        )
        .await;
    assert!(matches!(second, CommandOutcome::NotIdle));
    assert_eq!(
        with_live(&hub, |live| usize::from(
            live.unsettled_user_message.is_some()
        ))
        .await,
        1,
        "the rejected task must not enter the hub ledger"
    );

    assert!(
        matches!(hub.fork(key(), None).await, ForkOutcome::NotIdle),
        "the first task still makes the session busy"
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

/// A live session is forked as live, so storage judges it by its checkpoints
/// rather than by a runtime snapshot that only describes the last shutdown.
#[tokio::test]
async fn forking_a_live_session_reports_it_as_live() {
    let (hub, opener) = hub_and_opener(TestOpener::new("reply", ToolApprovalMode::Auto));
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
        [(ForkCut::All, ForkSource::Live)],
        "a live source is judged by its checkpoints, not by a stale snapshot"
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
        async move {
            hub.attach(
                key(),
                1,
                "prov".into(),
                None,
                PermissionMode::default(),
                false,
            )
            .await
        }
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

/// `ThreadBusy` used to mean different things depending on who was asking: with
/// a runtime attached it was read as a checkpoint still in flight and the client
/// was told to retry. A turn is announced only once its content is stored, so
/// there is no such write to wait for any more — a thread parked mid-turn is
/// parked, and both callers get the same refusal.
#[tokio::test]
async fn a_busy_thread_refuses_the_same_way_live_or_cold() {
    let busy = ForkError::ThreadBusy {
        thread_id: "s1".into(),
    };

    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.fork_error = Some(busy.clone());
    let (cold_hub, _) = hub_and_opener(opener);
    assert!(matches!(
        cold_hub.fork(key(), None).await,
        ForkOutcome::NotIdle
    ));

    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.fork_error = Some(busy);
    let (live_hub, _) = hub_and_opener(opener);
    let _attach = live_hub
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
        live_hub.fork(key(), None).await,
        ForkOutcome::NotIdle
    ));
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

fn root_answered(event: &RelayEvent) -> bool {
    matches!(event, RelayEvent::Event(e)
        if matches!(&**e, WireEvent::LlmEnd { agent_name, message, .. }
            if agent_name == "coda" && message.tool_calls.is_empty()))
}

fn explore_started(event: &RelayEvent) -> bool {
    matches!(event, RelayEvent::Event(e)
        if matches!(&**e, WireEvent::LlmStart { agent_name, .. } if agent_name == "explore"))
}

/// A session delegating to a sub-agent whose checkpoint write is slow: whatever
/// settles has to have waited for it, or a copy taken straight afterwards would
/// be read from a database that is behind.
async fn slow_sub_agent_session() -> (SessionHub, BoxStream<'static, RelayEvent>) {
    let hub = SessionHub::new(
        Arc::new(TestOpener::delegating(
            "reply",
            Some(Duration::from_millis(400)),
        )),
        RelayConfig::default(),
    );
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
    (hub, attach.events)
}

async fn send_task(hub: &SessionHub, task: &str) {
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: task.into(),
            images: vec![],
        },
    )
    .await;
}

// Forking used to need a retry: the client could see a turn finish before the
// database had it, so the copy came back "not stored yet" and the client sent it
// again. A turn is now announced only once its content is durable, on every
// path — so each of the three moments that used to lose that race succeeds first
// time. One session each, because the copy is taken the instant the turn ends.

#[tokio::test(flavor = "multi_thread")]
async fn forking_the_moment_a_turn_ends_succeeds_first_time() {
    let (hub, mut events) = slow_sub_agent_session().await;
    send_task(&hub, "go").await;
    next_matching(&mut events, root_answered).await;
    wait_idle(&hub).await;

    assert!(matches!(
        hub.fork(key(), None).await,
        ForkOutcome::Forked(_)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn forking_the_moment_an_abort_finishes_succeeds_first_time() {
    let (hub, mut events) = slow_sub_agent_session().await;
    send_task(&hub, "go").await;
    next_matching(&mut events, explore_started).await;

    hub.command(key(), 1, SessionCommand::Abort).await;
    next_matching(&mut events, |event| {
        matches!(event, RelayEvent::Event(e)
            if matches!(&**e, WireEvent::Aborted { agent_name, .. } if agent_name == "coda"))
    })
    .await;
    wait_idle(&hub).await;

    assert!(matches!(
        hub.fork(key(), None).await,
        ForkOutcome::Forked(_)
    ));
}
