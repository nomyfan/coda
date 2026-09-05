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
            coda_process::TaskExit::Exited { code: Some(0) }
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
    assert!(
        background
            .spawn_with(task_meta("too late"), |_ctx| async {
                coda_process::TaskExit::Exited { code: Some(0) }
            })
            .await
            .is_err(),
        "detach release left the external registry open"
    );
    hub.shutdown_all().await;
}

/// A completion can publish while the notice watcher is blocked on the entry
/// lock. The release check itself must take the registry notice before it
/// trusts the zero running count.
#[tokio::test(flavor = "multi_thread")]
async fn release_check_cannot_overtake_a_published_completion_notice() {
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
    let finish = Arc::new(Notify::new());
    let task_finish = finish.clone();
    background
        .spawn_with(task_meta("race"), move |_ctx| async move {
            task_finish.notified().await;
            coda_process::TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();

    let entry = hub.get_entry(&key()).unwrap();
    let mut guard = entry.inner.clone().lock_owned().await;
    guard.attached = None;
    let mut summaries = background.summaries();
    finish.notify_one();
    timeout(Duration::from_secs(5), async {
        while summaries
            .borrow_and_update()
            .iter()
            .any(|task| task.status.is_running())
        {
            summaries.changed().await.unwrap();
        }
    })
    .await
    .expect("task did not settle");

    assert!(
        SessionHub::maybe_release(&hub.entries, &entry, &mut guard)
            .await
            .is_none(),
        "release overtook the watcher and dropped the completion"
    );
    assert_eq!(guard.pending_notices.len(), 1);
    drop(guard);
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_all_keeps_the_entry_until_registry_shutdown_finishes() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let hub = Arc::new(hub);
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
        .unwrap();
    let background = background_of(&hub).await;
    let cancelled = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let task_cancelled = cancelled.clone();
    let task_finish = finish.clone();
    background
        .spawn_with(task_meta("held shutdown"), move |ctx| async move {
            ctx.cancelled().cancelled().await;
            task_cancelled.notify_one();
            task_finish.notified().await;
            coda_process::TaskExit::Killed
        })
        .await
        .unwrap();

    let shutdown_hub = hub.clone();
    let mut shutdown = tokio::spawn(async move { shutdown_hub.shutdown_all().await });
    timeout(Duration::from_secs(5), cancelled.notified())
        .await
        .expect("registry shutdown did not cancel the task");
    assert!(
        hub.get_entry(&key()).is_some(),
        "map entry removed too early"
    );
    assert!(
        timeout(Duration::from_millis(50), &mut shutdown)
            .await
            .is_err(),
        "shutdown_all returned before the monitor joined"
    );
    finish.notify_one();
    timeout(Duration::from_secs(5), shutdown)
        .await
        .expect("shutdown_all did not finish")
        .unwrap();
    assert!(hub.get_entry(&key()).is_none());
    assert!(
        background
            .spawn_with(task_meta("too late"), |_ctx| async {
                coda_process::TaskExit::Exited { code: Some(0) }
            })
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_all_waits_for_an_in_flight_delete() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let hub = Arc::new(hub);
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
        .unwrap();
    let background = background_of(&hub).await;
    let cancelled = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let task_cancelled = cancelled.clone();
    let task_finish = finish.clone();
    background
        .spawn_with(task_meta("delete barrier"), move |ctx| async move {
            ctx.cancelled().cancelled().await;
            task_cancelled.notify_one();
            task_finish.notified().await;
            coda_process::TaskExit::Killed
        })
        .await
        .unwrap();

    let delete_hub = hub.clone();
    let delete = tokio::spawn(async move { delete_hub.delete(key(), 1).await });
    timeout(Duration::from_secs(5), cancelled.notified())
        .await
        .expect("delete did not enter registry shutdown");
    let shutdown_hub = hub.clone();
    let mut shutdown = tokio::spawn(async move { shutdown_hub.shutdown_all().await });
    assert!(
        timeout(Duration::from_millis(50), &mut shutdown)
            .await
            .is_err(),
        "shutdown_all skipped an entry with a delete in flight"
    );
    assert!(
        hub.get_entry(&key()).is_some(),
        "map entry removed too early"
    );

    finish.notify_one();
    assert!(matches!(
        timeout(Duration::from_secs(5), delete)
            .await
            .expect("delete did not finish")
            .unwrap(),
        DeleteOutcome::Deleted
    ));
    timeout(Duration::from_secs(5), shutdown)
        .await
        .expect("shutdown_all did not observe release completion")
        .unwrap();
    assert!(hub.get_entry(&key()).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_ended_release_closes_the_external_registry_before_map_removal() {
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
        .unwrap();
    let background = background_of(&hub).await;
    let entry = hub.get_entry(&key()).unwrap();
    let generation = with_live(&hub, |live| live.generation).await;
    let cancelled = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let task_cancelled = cancelled.clone();
    let task_finish = finish.clone();
    background
        .spawn_with(task_meta("stream end"), move |ctx| async move {
            ctx.cancelled().cancelled().await;
            task_cancelled.notify_one();
            task_finish.notified().await;
            coda_process::TaskExit::Killed
        })
        .await
        .unwrap();

    let (tx, rx) = mpsc::unbounded_channel();
    drop(tx);
    tokio::spawn(run_forwarder(
        hub.entries.clone(),
        entry,
        rx,
        "coda".into(),
        generation,
        hub.opener.clone(),
        hub.status_tx.clone(),
    ));
    timeout(Duration::from_secs(5), cancelled.notified())
        .await
        .expect("stream-ended release did not close the registry");
    assert!(
        hub.get_entry(&key()).is_some(),
        "map entry removed too early"
    );
    finish.notify_one();
    wait_released(&hub).await;
    assert!(
        background
            .spawn_with(task_meta("too late"), |_ctx| async {
                coda_process::TaskExit::Exited { code: Some(0) }
            })
            .await
            .is_err()
    );
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
            coda_process::TaskExit::Exited { code: Some(0) }
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
            coda_process::TaskExit::Exited { code: Some(0) }
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
            coda_process::TaskExit::Exited { code: Some(0) }
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
            coda_process::TaskExit::Killed
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
                coda_process::TaskExit::Exited { code: Some(0) }
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

async fn root_reads_terminal_results(include_unread: bool, subagents: bool) {
    timeout(Duration::from_secs(5), async {
        let (hub, gate) = hub_with("read-task-results", ToolApprovalMode::Auto);
        let _attachment = hub
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
        let background = background_of(&hub).await;
        let meta = |name: &str| {
            let mut meta = task_meta(name);
            if subagents {
                meta.kind = coda_process::TaskKind::Subagent {
                    agent_name: name.into(),
                };
            }
            meta
        };
        let finish = Arc::new(Notify::new());
        let release = finish.clone();
        let normal = background
            .spawn_with(meta("normal"), move |ctx| async move {
                release.notified().await;
                ctx.append_stdout(b"complete output").await.unwrap();
                if subagents {
                    coda_process::TaskExit::Completed {
                        answer: "complete subagent answer".into(),
                    }
                } else {
                    coda_process::TaskExit::Exited { code: Some(0) }
                }
            })
            .await
            .unwrap();
        let killed = background
            .spawn_with(meta("killed"), |ctx| async move {
                ctx.cancelled().cancelled().await;
                ctx.append_stdout(b"partial output before kill")
                    .await
                    .unwrap();
                coda_process::TaskExit::Killed
            })
            .await
            .unwrap();
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: serde_json::to_string(&vec![normal.to_string(), killed.to_string()]).unwrap(),
                images: vec![],
            },
        )
        .await;
        finish.notify_one();
        background.wait_terminal(&normal).await;
        background.kill(&killed).await.unwrap();
        let unread = if include_unread {
            let id = background
                .spawn_with(task_meta("unread"), |_| async {
                    coda_process::TaskExit::Exited { code: Some(0) }
                })
                .await
                .unwrap();
            background.wait_terminal(&id).await;
            Some(id)
        } else {
            None
        };
        gate.notify_one();
        wait_idle(&hub).await;
        if subagents {
            while !background.take_notices().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        let notice = notice_message_in_view(&hub).await;
        if let Some(unread) = unread {
            let notice = notice.expect("the unread completion must still trigger a turn");
            assert_eq!(notice.outcomes.len(), 1);
            assert!(notice.content.contains(unread.as_str()));
            assert!(!notice.content.contains(normal.as_str()));
            assert!(!notice.content.contains(killed.as_str()));
        } else {
            assert!(
                notice.is_none(),
                "reading both results must not trigger a redundant turn"
            );
        }
        let entry = hub.get_entry(&key()).unwrap();
        let guard = entry.inner.lock().await;
        let EntryPhase::Live(live) = &guard.phase else {
            panic!("live session")
        };
        assert!(live.session.has_task_notice_receipt(normal).await.unwrap());
        assert!(live.session.has_task_notice_receipt(killed).await.unwrap());
        drop(guard);
        hub.shutdown_all().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_terminal_status_after_draining_running_output_suppresses_notice() {
    let (hub, gate) = hub_with("read-running-task-result", ToolApprovalMode::Auto);
    let mut attachment = hub
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
    let background = background_of(&hub).await;
    let ready = Arc::new(Notify::new());
    let written = ready.clone();
    let finish = Arc::new(Notify::new());
    let release = finish.clone();
    let id = background
        .spawn_with(task_meta("read before exit"), move |ctx| async move {
            ctx.append_stdout(b"all output before exit").await.unwrap();
            written.notify_one();
            release.notified().await;
            coda_process::TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    ready.notified().await;
    hub.command(
        key(),
        1,
        SessionCommand::Task {
            task: serde_json::to_string(&vec![id.to_string()]).unwrap(),
            images: vec![],
        },
    )
    .await;
    gate.notify_one();
    next_matching(&mut attachment.events, |event| {
        matches!(
            event,
            RelayEvent::Event(e) if matches!(&**e, WireEvent::ToolCallEnd { message, .. }
                if message.name == "task_output")
        )
    })
    .await;
    finish.notify_one();
    background.wait_terminal(&id).await;
    gate.notify_one();
    wait_idle(&hub).await;
    assert!(notice_message_in_view(&hub).await.is_none());
    let entry = hub.get_entry(&key()).unwrap();
    let guard = entry.inner.lock().await;
    let EntryPhase::Live(live) = &guard.phase else {
        panic!("live session")
    };
    assert!(live.session.has_task_notice_receipt(id).await.unwrap());
    drop(guard);
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_completed_and_killed_shell_results_suppresses_the_extra_notice_turn() {
    root_reads_terminal_results(false, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_notice_batch_keeps_only_the_task_root_has_not_read() {
    root_reads_terminal_results(true, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_terminal_subagent_results_acknowledges_the_archive_without_an_extra_turn() {
    root_reads_terminal_results(false, true).await;
}
