//! `SetModel` dispatch: no-op on unchanged selection, effort switches,
//! provider/model lock, persistence failure, and the running/unattached
//! guards.

use super::super::*;
use super::fixtures::*;
use coda_agent::ToolApprovalMode;

#[tokio::test(flavor = "multi_thread")]
async fn set_model_to_current_selection_is_unchanged() {
    // Re-selecting the model already in effect is a benign no-op the dispatcher
    // reports as idempotent success (Decision 8).
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionPreset::default(),
            false,
        )
        .await
        .expect("attach");

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::SetModel {
                provider_id: "prov".into(),
                reasoning_effort: None,
            },
        )
        .await,
        CommandOutcome::Unchanged
    ));

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_effort_switch_returns_model_changed() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionPreset::default(),
            false,
        )
        .await
        .expect("attach");

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
        CommandOutcome::ModelChanged { provider_id, reasoning_effort }
            if provider_id == "prov" && reasoning_effort.as_deref() == Some("high")
    ));

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_rejects_a_different_provider_or_model() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(
            key(),
            1,
            "prov:model-a".into(),
            None,
            PermissionPreset::default(),
            false,
        )
        .await
        .expect("attach");

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::SetModel {
                provider_id: "prov:model-b".into(),
                reasoning_effort: None,
            },
        )
        .await,
        CommandOutcome::ModelLocked
    ));

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_effort_persistence_keeps_live_selection() {
    let hub = hub_with_failing_metadata("reply");
    let _attach = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionPreset::default(),
            false,
        )
        .await
        .expect("attach");

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
        CommandOutcome::PersistenceFailed(ref error)
            if error == "injected metadata write failure"
    ));
    let refreshed = hub
        .attach(
            key(),
            1,
            "prov".into(),
            Some("high".into()),
            PermissionPreset::default(),
            false,
        )
        .await
        .expect("refresh attach");
    assert_eq!(refreshed.snapshot.provider_id, "prov");
    assert_eq!(refreshed.snapshot.reasoning_effort, None);

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_while_turn_running_is_rejected() {
    // A live session can only be rebuilt while idle; a switch during an
    // in-flight turn is a soft reject (→ MODEL_SWITCH_WHILE_RUNNING), not a
    // silent `Ignored` that the dispatcher would misread as SESSION_NOT_LIVE.
    let (hub, gate) = hub_with("hold", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionPreset::default(),
            false,
        )
        .await
        .expect("attach");

    // `handle_task` flips `turn_running` synchronously once the session accepts
    // the task, so the following `set_model` observes a running turn.
    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "hold on".into(),
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
            SessionCommand::SetModel {
                provider_id: "other".into(),
                reasoning_effort: None,
            },
        )
        .await,
        CommandOutcome::TurnRunning
    ));

    // Let the held turn settle so shutdown is prompt.
    gate.notify_one();
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_on_unattached_connection_is_ignored() {
    // The stale/not-attached guard in `command` returns `Ignored` *before*
    // dispatch; the request layer reads that as SESSION_NOT_LIVE.
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionPreset::default(),
            false,
        )
        .await
        .expect("attach");

    // Connection 2 never attached: its command is refused at the guard.
    assert!(matches!(
        hub.command(
            key(),
            2,
            SessionCommand::SetModel {
                provider_id: "other".into(),
                reasoning_effort: None,
            },
        )
        .await,
        CommandOutcome::Ignored
    ));

    hub.shutdown_all().await;
}
