//! Background tasks against the entry lifecycle: what keeps a session alive,
//! and how a finished task gets itself back in front of the model.

use super::super::*;
use super::fixtures::*;
use coda_agent::ToolApprovalMode;
use coda_core::llm::TaskNoticeMessage;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};

/// Whether a task notice is anywhere in the entry's view of the conversation.
/// A notice that just opened a turn sits in `unsettled_user_message` until that
/// turn of its own settles, which the gated provider in these tests may never
/// let happen.
async fn notice_in_view(hub: &SessionHub) -> Option<String> {
    notice_message_in_view(hub).await.map(|n| n.content)
}

/// The notice message itself, for the assertions that are about what it
/// carries rather than what it says.
async fn notice_message_in_view(hub: &SessionHub) -> Option<TaskNoticeMessage> {
    with_live(hub, |live| {
        let unsettled = live
            .unsettled_user_message
            .iter()
            .map(|(_, message)| message);
        live.snapshot.iter().chain(unsettled).find_map(|m| match m {
            Message::TaskNotice(notice) => Some(notice.clone()),
            _ => None,
        })
    })
    .await
}

/// An unattached session with no turn running is released — unless a
/// background task is still going. Nobody else is holding the process, and a
/// released entry could neither report the task's ending nor be asked to kill
/// it.
#[tokio::test(flavor = "multi_thread")]
async fn a_running_task_keeps_an_unattached_entry_alive() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _events = hub
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

    let background = background_of(&hub).await;
    let release = Arc::new(Notify::new());
    let held = release.clone();
    background
        .spawn_with(task_meta("sleep"), move |_ctx| async move {
            held.notified().await;
            coda_background::TaskExit::Exited { code: Some(0) }
        })
        .await
        .expect("spawn");

    hub.detach(key(), 1).await;

    // Give the release path every chance to run before concluding it didn't.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert!(
        hub.get_entry(&key()).is_some(),
        "the entry was released with a task still running"
    );

    // `notify_one` rather than `notify_waiters`: the task may not have reached
    // its await yet, and a notification with nobody registered is simply lost.
    release.notify_one();
    wait_released(&hub).await;
    hub.shutdown_all().await;
}

/// A task that finishes gets the model's attention on its own: the notice
/// opens a turn, and it is recorded as written by the runtime rather than by
/// the user.
#[tokio::test(flavor = "multi_thread")]
async fn a_finished_task_opens_a_turn_of_its_own() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _events = hub
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

    let background = background_of(&hub).await;
    background
        .spawn_with(task_meta("echo hi"), |_ctx| async {
            coda_background::TaskExit::Exited { code: Some(0) }
        })
        .await
        .expect("spawn");

    let notice = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(text) = notice_in_view(&hub).await {
                return text;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the finished task never reached the model");

    assert!(
        notice.contains("finished") && notice.contains("exited with code 0"),
        "unexpected notice text: {notice}"
    );
    hub.shutdown_all().await;
}

/// One turn at a time: a task that lands mid-turn waits for the turn in flight
/// to end rather than interrupting it, and is delivered right after.
#[tokio::test(flavor = "multi_thread")]
async fn a_notice_arriving_mid_turn_waits_for_the_turn_to_end() {
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

    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "work".into(),
            images: vec![],
        },
    )
    .await;

    let background = background_of(&hub).await;
    background
        .spawn_with(task_meta("echo hi"), |_ctx| async {
            coda_background::TaskExit::Exited { code: Some(0) }
        })
        .await
        .expect("spawn");

    // The turn is parked in the provider; the notice cannot have landed yet.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert!(
        notice_in_view(&hub).await.is_none(),
        "the notice interrupted a running turn"
    );

    gate.notify_one();
    next_matching(&mut events, is_settling_llm_end).await;

    timeout(Duration::from_secs(5), async {
        loop {
            if notice_in_view(&hub).await.is_some() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the notice was never delivered after the turn ended");

    // Let the notice's own turn finish too, or the teardown below sits out the
    // full graceful deadline waiting for a provider that is still parked.
    gate.notify_one();
    wait_idle(&hub).await;
    hub.shutdown_all().await;
}

/// Switching models rebuilds the runtime, but the tasks belong to the session:
/// the replacement adopts the registry the outgoing one was started with.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_switch_keeps_the_running_tasks() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _events = hub
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

    let before = background_of(&hub).await;
    let release = Arc::new(Notify::new());
    let held = release.clone();
    let id = before
        .spawn_with(task_meta("sleep"), move |_ctx| async move {
            held.notified().await;
            coda_background::TaskExit::Exited { code: Some(0) }
        })
        .await
        .expect("spawn");

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
        CommandOutcome::ModelChanged { .. }
    ));

    let after = background_of(&hub).await;
    assert!(
        Arc::ptr_eq(&before, &after),
        "the model switch replaced the task registry"
    );
    assert!(
        after
            .read(&id)
            .await
            .expect("registry readable")
            .is_some_and(|read| read.status.is_running()),
        "the task did not survive the model switch"
    );

    release.notify_one();
    hub.shutdown_all().await;
}

/// The user can stop a task themselves: killing needs no live turn and no
/// agreement from the model, since the registry belongs to the entry rather
/// than to whatever the runtime is doing.
#[tokio::test(flavor = "multi_thread")]
async fn killing_a_task_from_the_client_settles_it() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _events = hub
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

    let background = background_of(&hub).await;
    let id = background
        .spawn_with(task_meta("sleep"), |ctx| async move {
            ctx.cancelled().cancelled().await;
            coda_background::TaskExit::Killed
        })
        .await
        .expect("spawn");

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::KillTask {
                task_id: id.to_string(),
            },
        )
        .await,
        CommandOutcome::Ok
    ));

    let read = background
        .read(&id)
        .await
        .expect("registry readable")
        .expect("task still known");
    assert_eq!(read.status.describe(), "killed");

    // Killing something already gone is not an error — the list the user
    // clicked from is allowed to be a moment stale — but an id that never
    // existed is.
    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::KillTask {
                task_id: id.to_string(),
            },
        )
        .await,
        CommandOutcome::Ok
    ));
    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::KillTask {
                task_id: "bg_00000000000000000000000000000000".into(),
            },
        )
        .await,
        CommandOutcome::Ignored
    ));

    hub.shutdown_all().await;
}

/// Three tasks finishing while one turn runs is one interruption, not three:
/// everything waiting when a turn frees up goes out together.
#[tokio::test(flavor = "multi_thread")]
async fn notices_that_pile_up_during_a_turn_arrive_as_one() {
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

    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: "work".into(),
            images: vec![],
        },
    )
    .await;

    let background = background_of(&hub).await;
    for i in 0..3 {
        background
            .spawn_with(task_meta(&format!("echo {i}")), |_ctx| async {
                coda_background::TaskExit::Exited { code: Some(0) }
            })
            .await
            .expect("spawn");
    }

    // All three must actually have finished before the turn frees up, or this
    // would be testing the timing rather than the merging.
    let mut summaries = background.summaries();
    timeout(Duration::from_secs(5), async {
        while summaries
            .borrow_and_update()
            .iter()
            .any(|t| t.status.is_running())
        {
            summaries.changed().await.expect("registry alive");
        }
    })
    .await
    .expect("tasks never settled");

    gate.notify_one();
    next_matching(&mut events, is_settling_llm_end).await;

    let notice = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(notice) = notice_message_in_view(&hub).await {
                return notice;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the notices were never delivered");

    assert_eq!(
        notice.outcomes.len(),
        3,
        "each task should be a fact of the one message, not a turn of its own"
    );
    for i in 0..3 {
        assert!(
            notice.content.contains(&format!("echo {i}")),
            "task {i} missing from the notice: {}",
            notice.content
        );
    }

    gate.notify_one();
    wait_idle(&hub).await;
    hub.shutdown_all().await;
}
