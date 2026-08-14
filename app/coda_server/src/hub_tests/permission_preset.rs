//! The session's permission preset: seeded by the attach that opens it,
//! authoritative for every later attach, and switchable live without rebuilding
//! the runtime.

use super::super::*;
use super::fixtures::*;
use coda_agent::ToolApprovalMode;

/// What the *running* session would now decide, read through the same cell its
/// approval closure holds. Asserting on this rather than on the snapshot is what
/// separates "the client was told" from "the runtime actually changed".
fn runtime_preset(opener: &TestOpener) -> PermissionPreset {
    opener
        .opened_presets
        .lock()
        .unwrap()
        .last()
        .expect("a session was opened")
        .get()
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_attach_seeds_the_preset() {
    let (hub, opener) = hub_and_opener(TestOpener::new("reply", ToolApprovalMode::Auto));
    let attach = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionPreset::Explore,
            false,
        )
        .await
        .expect("attach");

    assert_eq!(attach.snapshot.permission_preset, PermissionPreset::Explore);
    assert_eq!(runtime_preset(&opener), PermissionPreset::Explore);

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_session_keeps_its_own_preset_across_a_takeover() {
    // The case this is really about: a client reconnecting to a session that is
    // already running must adopt what the session is executing under, not
    // impose whatever it happened to remember. Otherwise switching browsers
    // could silently loosen — or tighten — a session mid-flight.
    let (hub, opener) = hub_and_opener(TestOpener::new("reply", ToolApprovalMode::Auto));
    let _first = hub
        .attach(key(), 1, "prov".into(), None, PermissionPreset::Yolo, false)
        .await
        .expect("attach");

    let second = hub
        .attach(
            key(),
            2,
            "prov".into(),
            None,
            PermissionPreset::Explore,
            true,
        )
        .await
        .expect("takeover");

    assert_eq!(second.snapshot.permission_preset, PermissionPreset::Yolo);
    assert_eq!(runtime_preset(&opener), PermissionPreset::Yolo);

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_permission_preset_reaches_the_running_runtime() {
    let (hub, opener) = hub_and_opener(TestOpener::new("reply", ToolApprovalMode::Auto));
    let _attach = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionPreset::Explore,
            false,
        )
        .await
        .expect("attach");

    assert!(matches!(
        hub.command(
            key(),
            1,
            SessionCommand::SetPermissionPreset {
                preset: PermissionPreset::Yolo,
            },
        )
        .await,
        CommandOutcome::Ok
    ));

    // One session, one cell: no rebuild, and the runtime already opened is the
    // one that sees the change.
    assert_eq!(opener.opened_presets.lock().unwrap().len(), 1);
    assert_eq!(runtime_preset(&opener), PermissionPreset::Yolo);

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_preset_survives_a_model_rebuild() {
    // `SetModel` opens a replacement runtime; it has to carry the posture over,
    // or changing the reasoning effort would quietly reset the session's
    // permissions.
    let (hub, opener) = hub_and_opener(TestOpener::new("reply", ToolApprovalMode::Auto));
    let _attach = hub
        .attach(key(), 1, "prov".into(), None, PermissionPreset::Yolo, false)
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
        CommandOutcome::ModelChanged { .. }
    ));

    assert_eq!(opener.opened_presets.lock().unwrap().len(), 2);
    assert_eq!(runtime_preset(&opener), PermissionPreset::Yolo);

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_released_session_takes_the_next_clients_preset() {
    // Nothing is persisted: once the session is released the client's memory is
    // the only record of its posture, and reopening restores from it.
    let (hub, opener) = hub_and_opener(TestOpener::new("reply", ToolApprovalMode::Auto));
    let _attach = hub
        .attach(key(), 1, "prov".into(), None, PermissionPreset::Yolo, false)
        .await
        .expect("attach");
    hub.detach(key(), 1).await;
    wait_released(&hub).await;

    let reopened = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionPreset::Explore,
            false,
        )
        .await
        .expect("reattach");

    assert_eq!(
        reopened.snapshot.permission_preset,
        PermissionPreset::Explore
    );
    assert_eq!(runtime_preset(&opener), PermissionPreset::Explore);

    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_permission_preset_from_a_stale_connection_is_ignored() {
    let (hub, opener) = hub_and_opener(TestOpener::new("reply", ToolApprovalMode::Auto));
    let _attach = hub
        .attach(
            key(),
            1,
            "prov".into(),
            None,
            PermissionPreset::Explore,
            false,
        )
        .await
        .expect("attach");

    // Connection 2 never attached, so it has no say over this session.
    assert!(matches!(
        hub.command(
            key(),
            2,
            SessionCommand::SetPermissionPreset {
                preset: PermissionPreset::Yolo,
            },
        )
        .await,
        CommandOutcome::Ignored
    ));
    assert_eq!(runtime_preset(&opener), PermissionPreset::Explore);

    hub.shutdown_all().await;
}
