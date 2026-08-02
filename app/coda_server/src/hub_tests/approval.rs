//! Suspend-for-approval lifecycle: surviving a release/reattach cycle and
//! being cleared out when superseded by a fresh task.

use super::super::*;
use super::fixtures::*;
use coda_agent::{ToolApprovalMode, ToolCallResolution};
use std::time::Duration;

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

/// One turn can settle twice: once when a sub-agent suspends for approval, and
/// again when the user stops answering and submits something else instead, which
/// winds that turn up for real. Off the front of a queue, the second settle
/// swallows the *new* turn's message and reports the session idle while the new
/// turn is only just starting — so the accounting is keyed by turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_settles_twice_does_not_swallow_the_one_behind_it() {
    let hub = SessionHub::new(
        Arc::new(TestOpener::delegating_for_approval(Some(
            Duration::from_millis(200),
        ))),
        RelayConfig::default(),
    );
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

    // The sub-agent suspends, which settles the turn for the first time.
    next_matching(&mut events, |event| {
        matches!(event, RelayEvent::Event(e) if matches!(&**e, WireEvent::Suspended { .. }))
    })
    .await;

    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "instead".into(),
            images: vec![],
        },
    )
    .await;

    // Winding the first turn up settles it a second time.
    next_matching(&mut events, |event| {
        matches!(event, RelayEvent::Event(e)
            if matches!(&**e, WireEvent::Aborted { agent_name, .. } if agent_name == "coda"))
    })
    .await;

    let (pending, running) = unsettled(&hub).await;
    assert_eq!(
        pending,
        vec!["instead".to_string()],
        "the wound-up turn took the new submission's message with it"
    );
    assert!(running, "the session reported itself idle mid-turn");

    // And the new turn still runs to its own end.
    next_matching(&mut events, |event| {
        matches!(event, RelayEvent::Event(e)
            if matches!(&**e, WireEvent::LlmEnd { agent_name, message, .. }
                if agent_name == "coda" && message.tool_calls.is_empty()))
    })
    .await;
    wait_idle(&hub).await;
    let (pending, running) = unsettled(&hub).await;
    assert!(
        pending.is_empty(),
        "a submission was never folded: {pending:?}"
    );
    assert!(!running);
}
