//! The unseen-outcome mechanism: classifying which wire event produced the
//! settle, marking only when nobody was attached to see it, skipping
//! suspensions (already covered by `has_pending_approval`), and clearing on
//! the next attach without racing a settle already in flight. Also covers
//! `running_sessions`, the live counterpart the catalog merges it with.

use super::super::*;
use super::fixtures::*;
use crate::wire::AbortedTargetWire;
use coda_agent::ToolApprovalMode;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};

// --- unseen_outcome_for (pure) ------------------------------------------

fn llm_end(agent: &str) -> WireEvent {
    WireEvent::LlmEnd {
        agent_name: agent.into(),
        thread_id: "t".into(),
        message: assistant("done"),
    }
}

fn aborted(agent: &str) -> WireEvent {
    WireEvent::Aborted {
        agent_name: agent.into(),
        thread_id: "t".into(),
        target: AbortedTargetWire::Generation,
    }
}

fn errored(agent: &str) -> WireEvent {
    WireEvent::Error {
        agent_name: agent.into(),
        thread_id: "t".into(),
        message: "boom".into(),
    }
}

#[test]
fn a_normal_end_classifies_as_completed() {
    assert_eq!(
        unseen_outcome_for(&llm_end("coda")),
        UnseenOutcome::Completed
    );
}

#[test]
fn an_abort_classifies_as_failed() {
    assert_eq!(unseen_outcome_for(&aborted("coda")), UnseenOutcome::Failed);
}

#[test]
fn an_error_classifies_as_failed() {
    assert_eq!(unseen_outcome_for(&errored("coda")), UnseenOutcome::Failed);
}

// --- end-to-end through the hub -----------------------------------------

fn content_chunk(event: &RelayEvent) -> bool {
    matches!(event, RelayEvent::Event(e) if matches!(&**e, WireEvent::LlmContentChunk { .. }))
}

#[tokio::test(flavor = "multi_thread")]
async fn settling_unattended_marks_completed_and_releases() {
    let (hub, opener, gate) = hub_opener_and_gate(TestOpener::new("hold", ToolApprovalMode::Auto));
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
    // The turn is genuinely in flight (blocked on `gate`) before we walk away.
    next_matching(&mut events, content_chunk).await;

    hub.detach(key(), 1).await;
    gate.notify_one();
    wait_released(&hub).await;

    assert_eq!(
        *opener.unseen_outcomes.lock().unwrap(),
        vec![(key(), None), (key(), Some(UnseenOutcome::Completed))],
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn settling_while_attached_does_not_mark() {
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
    next_matching(&mut events, is_settling_llm_end).await;
    wait_idle(&hub).await;

    // Only the initial attach's clear — the settle itself never marked
    // anything, because someone was attached the whole time.
    assert_eq!(*opener.unseen_outcomes.lock().unwrap(), vec![(key(), None)]);

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn suspending_while_unattended_does_not_mark() {
    // `has_pending_approval` already covers this case; marking it here too
    // would just be a second, redundant indicator for the same fact.
    let (hub, opener, gate) =
        hub_opener_and_gate(TestOpener::new("approval_hold", ToolApprovalMode::Manual));
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
            task: "needs approval".into(),
            images: vec![],
        },
    )
    .await;
    next_matching(&mut events, content_chunk).await;

    hub.detach(key(), 1).await;
    gate.notify_one();
    wait_released(&hub).await;

    assert_eq!(*opener.unseen_outcomes.lock().unwrap(), vec![(key(), None)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_cannot_land_between_the_unattended_check_and_the_write() {
    // The entry lock is held across `mark_unseen_outcome`'s write, so an
    // attach racing a settle already in flight cannot observe (or produce) a
    // session that is briefly "attached but marked unseen". Proven by
    // stalling the write behind a gate: a concurrent attach must wait for it,
    // and the recorded order must be mark-then-clear, never the reverse.
    let mark_gate = Arc::new(Notify::new());
    let mut opener = TestOpener::new("hold", ToolApprovalMode::Auto);
    opener.mark_unseen_gate = Some(mark_gate.clone());
    let (hub, opener, hold_gate) = hub_opener_and_gate(opener);

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
    next_matching(&mut events1, content_chunk).await;
    hub.detach(key(), 1).await;

    // Let the turn settle. `mark_unseen_entered` fires the instant the
    // forwarder reaches the write and stalls on `mark_gate` — waiting for it
    // (rather than guessing at timing) is what makes the race below
    // deterministic: the forwarder is provably still holding the entry lock
    // once this returns.
    hold_gate.notify_one();
    opener.mark_unseen_entered.notified().await;

    let mut attach2 = {
        let hub = hub.clone();
        tokio::spawn(async move {
            hub.attach(
                key(),
                2,
                "prov".into(),
                None,
                PermissionMode::default(),
                true,
            )
            .await
        })
    };
    assert!(
        timeout(Duration::from_millis(200), &mut attach2)
            .await
            .is_err(),
        "attach completed while the unattended write was still stalled",
    );

    mark_gate.notify_one();
    attach2
        .await
        .expect("attach task panicked")
        .expect("re-attach");

    assert_eq!(
        *opener.unseen_outcomes.lock().unwrap(),
        vec![
            (key(), None),                           // attach 1
            (key(), Some(UnseenOutcome::Completed)), // the unattended settle
            (key(), None),                           // attach 2's clear
        ],
    );
}

// --- running_sessions ----------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn running_sessions_reports_only_running_sessions_in_the_given_workspace() {
    let (hub, _, gate) = hub_opener_and_gate(TestOpener::new("hold", ToolApprovalMode::Auto));
    let running: SessionKey = ("ws-a".into(), "running".into());
    let idle: SessionKey = ("ws-a".into(), "idle".into());

    let attach_running = hub
        .attach(
            running.clone(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
        .await
        .expect("attach");
    let mut running_events = attach_running.events;
    hub.command(
        running.clone(),
        1,
        SessionCommand::Task {
            task: "go".into(),
            images: vec![],
        },
    )
    .await;
    next_matching(&mut running_events, content_chunk).await;

    hub.attach(
        idle.clone(),
        2,
        "prov".into(),
        None,
        PermissionMode::default(),
        false,
    )
    .await
    .expect("attach");

    assert_eq!(
        hub.running_sessions("ws-a").await,
        HashSet::from(["running".to_string()]),
    );
    // Same session id, different workspace: not the same session.
    assert!(hub.running_sessions("ws-b").await.is_empty());

    gate.notify_one();
    hub.shutdown_all().await;
}
