//! `Compact` command: its unlocked LLM window, entry-level busy gate, snapshot
//! lifecycle, and the distinction between recorded and unwritten failures.

use super::super::*;
use super::fixtures::*;
use coda_agent::ToolApprovalMode;
use futures::StreamExt;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};

fn is_snapshot(event: &RelayEvent, compacting: bool) -> bool {
    matches!(event, RelayEvent::Snapshot(snapshot) if snapshot.compacting == compacting)
}

async fn attached_compaction(
    result: Result<bool, CompactError>,
) -> (
    Arc<SessionHub>,
    Arc<TestOpener>,
    BoxStream<'static, RelayEvent>,
) {
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.compact_result = result;
    let (hub, opener) = hub_and_opener(opener);
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
    (hub, opener, attach.events)
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_stays_attachable_but_gates_every_history_mutation() {
    let gate = Arc::new(Notify::new());
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.compact_gate = Some(gate.clone());
    let (hub, opener) = hub_and_opener(opener);
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
    let mut old_events = attach.events;

    let compact = {
        let hub = hub.clone();
        tokio::spawn(async move {
            hub.command(
                key(),
                1,
                SessionCommand::Compact {
                    instructions: "keep decisions".into(),
                },
            )
            .await
        })
    };
    next_matching(&mut old_events, |event| is_snapshot(event, true)).await;

    let takeover = hub
        .attach(
            key(),
            2,
            "ignored".into(),
            Some("ignored".into()),
            PermissionMode::default(),
            true,
        )
        .await
        .expect("take over during compaction");
    assert!(takeover.snapshot.compacting);
    let mut events = takeover.events;
    next_matching(&mut old_events, |event| {
        matches!(event, RelayEvent::Evicted)
    })
    .await;

    assert!(matches!(
        hub.command(
            key(),
            2,
            SessionCommand::Task {
                task: "too soon".into(),
                images: vec![],
            },
        )
        .await,
        CommandOutcome::NotIdle
    ));
    assert!(matches!(
        hub.command(
            key(),
            2,
            SessionCommand::SetModel {
                provider_id: "prov".into(),
                reasoning_effort: None,
            },
        )
        .await,
        CommandOutcome::TurnRunning
    ));
    assert!(matches!(
        hub.command(
            key(),
            2,
            SessionCommand::Rewind {
                target: MessageId::new(),
                task: "replacement".into(),
                images: vec![],
            },
        )
        .await,
        CommandOutcome::NotIdle
    ));
    assert!(matches!(hub.fork(key(), None).await, ForkOutcome::NotIdle));
    assert!(opener.forks.lock().unwrap().is_empty());
    assert!(matches!(
        hub.command(
            key(),
            2,
            SessionCommand::Compact {
                instructions: String::new(),
            },
        )
        .await,
        CommandOutcome::NotIdle
    ));

    gate.notify_one();
    assert!(matches!(
        compact.await.expect("compact task"),
        CommandOutcome::Compacted { applied: true }
    ));
    let RelayEvent::Snapshot(finished) =
        next_matching(&mut events, |event| is_snapshot(event, false)).await
    else {
        unreachable!()
    };
    assert_eq!(finished.messages.len(), 2);
    assert!(
        matches!(&finished.messages[1], Message::Custom(message) if message.kind == "compaction")
    );

    // A snapshot event is not terminal: the same stream carries the next turn.
    assert!(matches!(
        hub.command(
            key(),
            2,
            SessionCommand::Task {
                task: "continue".into(),
                images: vec![],
            },
        )
        .await,
        CommandOutcome::TaskAccepted { .. }
    ));
    next_matching(&mut events, is_settling_llm_end).await;
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn detach_during_compaction_releases_only_after_it_finishes() {
    let gate = Arc::new(Notify::new());
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.compact_gate = Some(gate.clone());
    let (hub, _) = hub_and_opener(opener);
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
    let compact = {
        let hub = hub.clone();
        tokio::spawn(async move {
            hub.command(
                key(),
                1,
                SessionCommand::Compact {
                    instructions: String::new(),
                },
            )
            .await
        })
    };
    next_matching(&mut events, |event| is_snapshot(event, true)).await;

    hub.detach(key(), 1).await;
    assert!(
        hub.get_entry(&key()).is_some(),
        "the compaction keeps its entry alive"
    );

    gate.notify_one();
    assert!(matches!(
        compact.await.expect("compact task"),
        CommandOutcome::Compacted { applied: true }
    ));
    wait_released(&hub).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_summary_failure_writes_history_without_moving_the_boundary() {
    let (hub, _opener, mut events) = attached_compaction(Ok(false)).await;

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Compact {
                instructions: "keep decisions".into(),
            },
        )
        .await,
        CommandOutcome::Compacted { applied: false }
    ));
    next_matching(&mut events, |event| is_snapshot(event, true)).await;
    let RelayEvent::Snapshot(finished) =
        next_matching(&mut events, |event| is_snapshot(event, false)).await
    else {
        unreachable!()
    };
    assert_eq!(finished.messages.len(), 2);
    assert!(
        matches!(&finished.messages[1], Message::Custom(message) if message.kind == "compaction_failed")
    );
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_compaction_writes_nothing_and_still_clears_busy() {
    let (hub, _opener, mut events) = attached_compaction(Err(CompactError::Stale)).await;

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Compact {
                instructions: String::new(),
            },
        )
        .await,
        CommandOutcome::CompactionAbandoned { stale: true, .. }
    ));
    next_matching(&mut events, |event| is_snapshot(event, true)).await;
    let RelayEvent::Snapshot(finished) =
        next_matching(&mut events, |event| is_snapshot(event, false)).await
    else {
        unreachable!()
    };
    assert!(finished.messages.is_empty());

    // There is no hidden terminal event after the closing snapshot.
    assert!(
        timeout(Duration::from_millis(20), events.next())
            .await
            .is_err()
    );
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_in_flight_refuses_compaction_without_writing_a_marker() {
    let (hub, gate) = hub_with("hold", ToolApprovalMode::Auto);
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
                task: "hold".into(),
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
            SessionCommand::Compact {
                instructions: String::new(),
            },
        )
        .await,
        CommandOutcome::NotIdle
    ));

    gate.notify_one();
    next_matching(&mut events, is_settling_llm_end).await;
    wait_idle(&hub).await;
    let refreshed = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionMode::default(),
            false,
        )
        .await
        .expect("refresh");
    assert_eq!(refreshed.snapshot.messages.len(), 2);
    assert!(
        !refreshed
            .snapshot
            .messages
            .iter()
            .any(|message| matches!(message, Message::Custom(_)))
    );
    hub.shutdown_all().await;
}
