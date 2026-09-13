use super::super::*;
use super::fixtures::*;
use coda_agent::ToolApprovalMode;
use std::sync::{Arc, atomic::Ordering};
use tokio::{
    sync::Notify,
    time::{Duration, timeout},
};

fn switch_to(model: &str) -> SessionCommand {
    SessionCommand::SetModel {
        provider_id: model.into(),
        reasoning_effort: Some("high".into()),
    }
}

async fn attach(hub: &SessionHub) -> AttachSession {
    hub.attach(
        key(),
        1,
        "p1:preview".into(),
        None,
        PermissionMode::Yolo,
        false,
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn committed_binding_survives_open_failure_and_same_target_retry() {
    let control = Arc::new(ModelSwitchControl::default());
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.model_switch = Some(control.clone());
    let (hub, opener) = hub_and_opener(opener);
    let mut attached = attach(&hub).await;
    let background = background_of(&hub).await;
    let release = Arc::new(Notify::new());
    let held = release.clone();
    let task = background
        .spawn_with(task_meta("held shell"), move |_| async move {
            held.notified().await;
            coda_execution::TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    control.fail_open.store(true, Ordering::SeqCst);
    let CommandOutcome::ModelChanged(snapshot) =
        hub.command(key(), 1, switch_to("p2:released")).await
    else {
        panic!("snapshot required")
    };
    assert_eq!(snapshot.provider_id, "p2:released");
    assert_eq!(snapshot.model_family.as_deref(), Some("f"));
    assert_eq!(snapshot.permission_mode, PermissionMode::Yolo);
    assert_eq!(
        snapshot.access,
        SessionAccess::ReadOnly {
            reason: ReadOnlyReason::RuntimeOpenFailed
        }
    );
    assert!(
        snapshot
            .runtime_open_error
            .unwrap()
            .contains("injected model open failure")
    );
    assert_eq!(
        control
            .saved
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .selection_key(),
        "p2:released"
    );
    assert_eq!(
        *opener.calls.lock().unwrap(),
        ["open", "write_binding", "open"]
    );
    assert!(Arc::ptr_eq(&background, &background_of(&hub).await));
    assert!(
        background
            .summaries()
            .borrow()
            .iter()
            .any(|s| s.id == task.to_string() && s.status.is_running())
    );
    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "blocked".into(),
                images: vec![]
            }
        )
        .await,
        CommandOutcome::ReadOnly(_)
    ));

    control.fail_open.store(false, Ordering::SeqCst);
    let CommandOutcome::ModelChanged(snapshot) =
        hub.command(key(), 1, switch_to("p2:released")).await
    else {
        panic!("same target must retry")
    };
    assert_eq!(snapshot.access, SessionAccess::ReadWrite);
    assert_eq!(snapshot.runtime_open_error, None);
    assert_eq!(
        opener
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == "write_binding")
            .count(),
        1
    );
    assert!(Arc::ptr_eq(&background, &background_of(&hub).await));
    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "continue".into(),
                images: vec![]
            }
        )
        .await,
        CommandOutcome::TaskAccepted { .. }
    ));
    let event = next_matching(&mut attached.events, is_settling_llm_end).await;
    let RelayEvent::Event(event) = event else {
        unreachable!()
    };
    let WireEvent::LlmEnd { message, .. } = &*event else {
        unreachable!()
    };
    let generation = message.generation.as_ref().unwrap();
    assert_eq!(generation.provider_id, "p2");
    assert_eq!(generation.model_id, "released");
    assert_eq!(generation.reasoning_effort.as_deref(), Some("high"));
    wait_idle(&hub).await;
    release.notify_one();
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn uncertain_commit_freezes_execution_until_confirmation_and_never_switches_back_implicitly()
{
    for rollback in [false, true] {
        let control = Arc::new(ModelSwitchControl::default());
        let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
        opener.model_switch = Some(control.clone());
        let (hub, opener) = hub_and_opener(opener);
        let _attached = attach(&hub).await;
        control.uncertain.store(true, Ordering::SeqCst);
        control.rollback.store(rollback, Ordering::SeqCst);
        control.fail_confirmation.store(true, Ordering::SeqCst);
        let CommandOutcome::ModelChanged(snapshot) =
            hub.command(key(), 1, switch_to("p2:released")).await
        else {
            panic!("frozen snapshot")
        };
        assert_eq!(
            snapshot.access,
            SessionAccess::ReadOnly {
                reason: ReadOnlyReason::BindingUnconfirmed
            }
        );
        assert_eq!(snapshot.provider_id, "p1:preview");
        assert!(matches!(
            hub.command(
                key(),
                1,
                SessionCommand::Task {
                    task: "blocked".into(),
                    images: vec![]
                }
            )
            .await,
            CommandOutcome::ReadOnly(_)
        ));
        assert!(matches!(
            hub.command(key(), 1, switch_to("p1:preview")).await,
            CommandOutcome::PersistenceFailed(_)
        ));
        assert_eq!(
            opener
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| **c == "open")
                .count(),
            1
        );
        control.fail_confirmation.store(false, Ordering::SeqCst);
        // Retry carries the last displayed model, but only confirms the attempted write.
        let CommandOutcome::ModelChanged(snapshot) =
            hub.command(key(), 1, switch_to("p1:preview")).await
        else {
            panic!("confirmed snapshot")
        };
        assert_eq!(snapshot.access, SessionAccess::ReadWrite);
        assert_eq!(
            snapshot.provider_id,
            if rollback {
                "p1:preview"
            } else {
                "p2:released"
            }
        );
        assert_eq!(
            opener
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| **c == "write_binding")
                .count(),
            1
        );
        hub.shutdown_all().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_confirmed_rollback_keeps_the_original_runtime() {
    let control = Arc::new(ModelSwitchControl::default());
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.model_switch = Some(control.clone());
    let (hub, opener) = hub_and_opener(opener);
    let _attached = attach(&hub).await;
    control.uncertain.store(true, Ordering::SeqCst);
    control.rollback.store(true, Ordering::SeqCst);
    assert!(matches!(
        hub.command(key(), 1, switch_to("p2:released")).await,
        CommandOutcome::PersistenceFailed(_)
    ));
    let snapshot = attach(&hub).await.snapshot;
    assert_eq!(snapshot.access, SessionAccess::ReadWrite);
    assert_eq!(snapshot.provider_id, "p1:preview");
    assert_eq!(
        *opener.calls.lock().unwrap(),
        ["open", "write_binding", "confirm_binding"]
    );
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_the_request_cannot_cancel_an_accepted_model_switch() {
    let gate = Arc::new(Notify::new());
    let control = Arc::new(ModelSwitchControl {
        write_gate: Some(gate.clone()),
        ..Default::default()
    });
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.model_switch = Some(control.clone());
    let (hub, _) = hub_and_opener(opener);
    let _attached = attach(&hub).await;
    let switching = hub.clone();
    let request =
        tokio::spawn(async move { switching.command(key(), 1, switch_to("p2:released")).await });
    timeout(Duration::from_secs(3), control.write_entered.notified())
        .await
        .unwrap();
    request.abort();
    assert!(matches!(request.await, Err(error) if error.is_cancelled()));
    gate.notify_one();
    let snapshot = timeout(Duration::from_secs(3), attach(&hub))
        .await
        .unwrap()
        .snapshot;
    assert_eq!(snapshot.provider_id, "p2:released");
    assert_eq!(snapshot.access, SessionAccess::ReadWrite);
    assert_eq!(
        control
            .saved
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .selection_key(),
        snapshot.provider_id
    );
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refreshing_the_subscription_after_switch_discards_old_events_and_preserves_new_ones() {
    use futures::{FutureExt, StreamExt};
    let control = Arc::new(ModelSwitchControl::default());
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.model_switch = Some(control);
    let (hub, _) = hub_and_opener(opener);
    let mut old_attachment = attach(&hub).await;
    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "first".into(),
                images: vec![],
            }
        )
        .await,
        CommandOutcome::TaskAccepted { .. }
    ));
    wait_idle(&hub).await;
    assert!(matches!(
        hub.command(key(), 1, switch_to("p2:released")).await,
        CommandOutcome::ModelChanged(_)
    ));
    // The RPC installs this replacement stream before sending its snapshot.
    let mut refreshed = attach(&hub).await;
    assert_eq!(refreshed.snapshot.provider_id, "p2:released");
    assert_eq!(refreshed.snapshot.messages.len(), 2);
    assert!(
        matches!(&refreshed.snapshot.messages[1], Message::Assistant(message)
        if message.generation.as_ref().unwrap().provider_id == "p1")
    );
    let stale = next_matching(&mut old_attachment.events, is_settling_llm_end).await;
    assert!(
        matches!(stale, RelayEvent::Event(event) if matches!(&*event,
        WireEvent::LlmEnd { message, .. } if message.generation.as_ref().unwrap().provider_id == "p1"))
    );
    assert!(
        refreshed.events.next().now_or_never().is_none(),
        "the refreshed stream cannot replay settled old messages"
    );
    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "second".into(),
                images: vec![],
            }
        )
        .await,
        CommandOutcome::TaskAccepted { .. }
    ));
    let new = next_matching(&mut refreshed.events, is_settling_llm_end).await;
    assert!(matches!(new, RelayEvent::Event(event) if matches!(&*event,
        WireEvent::LlmEnd { message, .. } if message.generation.as_ref().unwrap().provider_id == "p2")));
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn confirming_rollback_to_a_removed_preview_restores_ordinary_read_only_selection() {
    let original = SessionModelBinding {
        provider_id: "p1".into(),
        model_id: "preview".into(),
        family: Some("f".into()),
        reasoning_effort: None,
    };
    let control = Arc::new(ModelSwitchControl {
        saved: std::sync::Mutex::new(Some(original.clone())),
        ..Default::default()
    });
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.model_switch = Some(control.clone());
    opener.unavailable_model = Some(UnavailableModel {
        binding: original.clone(),
        reason: ReadOnlyReason::ModelNotConfigured,
    });
    let (hub, opener) = hub_and_opener(opener);
    assert_eq!(
        attach(&hub).await.snapshot.access,
        SessionAccess::ReadOnly {
            reason: ReadOnlyReason::ModelNotConfigured
        }
    );
    control.uncertain.store(true, Ordering::SeqCst);
    control.rollback.store(true, Ordering::SeqCst);
    control.fail_confirmation.store(true, Ordering::SeqCst);
    let CommandOutcome::ModelChanged(frozen) =
        hub.command(key(), 1, switch_to("p2:released")).await
    else {
        panic!("frozen snapshot required")
    };
    assert_eq!(
        frozen.access,
        SessionAccess::ReadOnly {
            reason: ReadOnlyReason::BindingUnconfirmed
        }
    );

    control.fail_confirmation.store(false, Ordering::SeqCst);
    let CommandOutcome::ModelChanged(confirmed) =
        hub.command(key(), 1, switch_to("p1:preview")).await
    else {
        panic!("confirmation must return a usable snapshot even when the original model is absent")
    };
    assert_eq!(
        confirmed.access,
        SessionAccess::ReadOnly {
            reason: ReadOnlyReason::ModelNotConfigured
        }
    );
    assert_eq!(confirmed.provider_id, "p1:preview");
    assert_eq!(confirmed.model_family.as_deref(), Some("f"));
    assert_eq!(confirmed.runtime_open_error, None);
    assert_eq!(*control.saved.lock().unwrap(), Some(original));
    assert_eq!(
        *opener.calls.lock().unwrap(),
        ["write_binding", "confirm_binding"]
    );

    control.uncertain.store(false, Ordering::SeqCst);
    control.rollback.store(false, Ordering::SeqCst);
    let CommandOutcome::ModelChanged(recovered) =
        hub.command(key(), 1, switch_to("p2:released")).await
    else {
        panic!("manual replacement must be selectable again")
    };
    assert_eq!(recovered.provider_id, "p2:released");
    assert_eq!(recovered.access, SessionAccess::ReadWrite);
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_errors_survive_disconnect_until_explicit_retry_without_background_work() {
    for uncertain in [false, true] {
        let control = Arc::new(ModelSwitchControl::default());
        let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
        opener.model_switch = Some(control.clone());
        let (hub, opener) = hub_and_opener(opener);
        let attached = attach(&hub).await;
        control.fail_open.store(!uncertain, Ordering::SeqCst);
        control.uncertain.store(uncertain, Ordering::SeqCst);
        control.fail_confirmation.store(uncertain, Ordering::SeqCst);
        let CommandOutcome::ModelChanged(failed) =
            hub.command(key(), 1, switch_to("p2:released")).await
        else {
            panic!("failed recovery snapshot required")
        };
        assert!(failed.background_tasks.is_empty());
        let calls = opener.calls.lock().unwrap().clone();
        let entry = hub.get_entry(&key()).unwrap();
        // Removing the external failure must not make reconnect an implicit retry.
        control.fail_open.store(false, Ordering::SeqCst);
        control.fail_confirmation.store(false, Ordering::SeqCst);
        drop(attached);
        hub.detach(key(), 1).await;
        let reattached = attach(&hub).await;
        assert_eq!(reattached.snapshot.access, failed.access);
        assert_eq!(reattached.snapshot.provider_id, failed.provider_id);
        assert_eq!(
            reattached.snapshot.runtime_open_error,
            failed.runtime_open_error
        );
        assert_eq!(*opener.calls.lock().unwrap(), calls);
        assert!(Arc::ptr_eq(&entry, &hub.get_entry(&key()).unwrap()));
        let CommandOutcome::ModelChanged(recovered) =
            hub.command(key(), 1, switch_to("p2:released")).await
        else {
            panic!("explicit retry must restore the runtime")
        };
        assert_eq!(recovered.access, SessionAccess::ReadWrite);
        assert_eq!(recovered.provider_id, "p2:released");
        drop(reattached);
        hub.detach(key(), 1).await;
        wait_released(&hub).await;
        hub.shutdown_all().await;
    }
}
