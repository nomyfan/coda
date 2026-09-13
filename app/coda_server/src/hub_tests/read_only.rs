use super::super::*;
use super::fixtures::*;
use crate::session_access::ReadOnlyReason;
use crate::storage::SessionModelBinding;
use coda_agent::{ToolApprovalMode, ToolCallResolution, runtime::SessionStorage};

fn unavailable() -> UnavailableModel {
    UnavailableModel {
        binding: SessionModelBinding {
            family: None,
            provider_id: "removed".into(),
            model_id: "model".into(),
            reasoning_effort: Some("old-effort".into()),
        },
        reason: ReadOnlyReason::ModelNotConfigured,
    }
}

async fn attach(
    hub: &SessionHub,
    conn: ConnId,
    takeover: bool,
) -> Result<AttachSession, AttachError> {
    hub.attach(
        key(),
        conn,
        "valid:default".into(),
        None,
        PermissionMode::Yolo,
        takeover,
    )
    .await
}

#[tokio::test]
async fn read_only_opens_preserve_history_and_approvals_and_reject_every_mutation() {
    let (running, opener) = hub_and_opener(TestOpener::new("approval", ToolApprovalMode::Manual));
    let mut events = attach(&running, 1, false).await.unwrap().events;
    running
        .command(
            key(),
            1,
            SessionCommand::Task {
                task: "saved prompt".into(),
                images: vec![],
            },
        )
        .await;
    next_matching(&mut events, |event| matches!(event, RelayEvent::Event(event) if matches!(&**event, WireEvent::Suspended { .. }))).await;
    running.detach(key(), 1).await;
    wait_released(&running).await;
    let before =
        serde_json::to_value(opener.storage.load_checkpoint(&key().1).await.unwrap()).unwrap();
    let mut reader = TestOpener::new("reply", ToolApprovalMode::Auto);
    reader.storage = opener.storage.clone();
    reader.unavailable_model = Some(unavailable());
    let (hub, reader) = hub_and_opener(reader);
    let snapshot = attach(&hub, 2, false).await.unwrap().snapshot;
    assert_eq!(
        snapshot.access,
        SessionAccess::ReadOnly {
            reason: ReadOnlyReason::ModelNotConfigured
        }
    );
    assert_eq!(snapshot.provider_id, "removed:model");
    assert_eq!(snapshot.reasoning_effort.as_deref(), Some("old-effort"));
    assert!(!snapshot.messages.is_empty());
    assert!(!snapshot.turn_running);
    let approval = &snapshot.pending_approvals[0];
    let commands = [
        SessionCommand::Task {
            task: "new prompt".into(),
            images: vec![],
        },
        SessionCommand::Resume {
            allow_patterns: vec![(approval.calls[0].id.clone(), "echo *".into())],
            agent_name: approval.agent_name.clone(),
            pid: approval.pid.clone(),
            decision: ResumeDecision {
                parent_message_id: approval.parent_message_id,
                resolutions: vec![(approval.calls[0].id.clone(), ToolCallResolution::Execute)],
            },
        },
        SessionCommand::Rewind {
            target: MessageId::new(),
            task: "changed".into(),
            images: vec![],
        },
        SessionCommand::Compact {
            instructions: "summarize".into(),
        },
        SessionCommand::SetPermissionMode {
            mode: PermissionMode::Explore,
        },
        SessionCommand::Abort,
        SessionCommand::KillTask {
            task_id: coda_execution::TaskId::new().to_string(),
        },
    ];
    for command in commands {
        assert!(matches!(
            hub.command(key(), 2, command).await,
            CommandOutcome::ReadOnly(_)
        ));
    }
    assert!(matches!(
        hub.fork(key(), None).await,
        ForkOutcome::ReadOnly(_)
    ));
    assert!(
        reader.opened_modes.lock().unwrap().is_empty(),
        "no runtime open"
    );
    let entry = hub.get_entry(&key()).unwrap();
    let state = entry.inner.lock().await;
    assert!(state.background.is_none());
    assert!(state.notice_watcher.is_none());
    assert!(state.pending_notices.is_empty());
    assert_eq!(state.permission_mode.get(), PermissionMode::Yolo);
    drop(state);
    assert_eq!(
        before,
        serde_json::to_value(reader.storage.load_checkpoint(&key().1).await.unwrap()).unwrap()
    );
    hub.detach(key(), 2).await;
    wait_released(&hub).await;
    assert!(
        matches!(hub.fork(key(), None).await, ForkOutcome::ReadOnly(_)),
        "cold fork must also refuse"
    );
    assert!(hub.get_entry(&key()).is_none());
    let reopened = attach(&hub, 3, false).await.unwrap();
    assert_eq!(reopened.snapshot.pending_approvals.len(), 1);
    hub.shutdown_all().await;
    // Restoring configuration uses the existing approvals-gated open behavior.
    assert_eq!(
        attach(&running, 4, false).await.unwrap().snapshot.access,
        SessionAccess::ReadWrite
    );
    running.shutdown_all().await;
}

#[tokio::test]
async fn read_only_attachment_ownership_release_and_delete_follow_normal_rules() {
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.unavailable_model = Some(unavailable());
    let (hub, opener) = hub_and_opener(opener);
    let mut first = attach(&hub, 1, false).await.unwrap().events;
    assert!(matches!(
        attach(&hub, 2, false).await,
        Err(AttachError::Busy)
    ));
    let _second = attach(&hub, 2, true).await.unwrap();
    next_matching(&mut first, |event| matches!(event, RelayEvent::Evicted)).await;
    assert!(matches!(
        hub.command(key(), 1, SessionCommand::Abort).await,
        CommandOutcome::Ignored
    ));
    assert!(matches!(
        hub.delete(key(), 1).await,
        DeleteOutcome::NotOwner
    ));
    assert!(matches!(hub.delete(key(), 2).await, DeleteOutcome::Deleted));
    assert_eq!(opener.deleted.lock().unwrap().as_slice(), &[key()]);
    assert!(hub.get_entry(&key()).is_none());
}

#[tokio::test]
async fn a_storage_failure_is_not_an_empty_read_only_conversation() {
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.unavailable_model = Some(unavailable());
    opener.fail_read_only_load = true;
    let (hub, opener) = hub_and_opener(opener);
    assert!(matches!(
        attach(&hub, 1, false).await,
        Err(AttachError::Open(OpenError::Storage(_)))
    ));
    wait_released(&hub).await;
    assert!(opener.opened_modes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ordinary_read_only_sessions_still_release_on_disconnect() {
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.unavailable_model = Some(unavailable());
    let (hub, opener) = hub_and_opener(opener);
    let attached = attach(&hub, 1, false).await.unwrap();
    assert_eq!(attached.snapshot.runtime_open_error, None);
    drop(attached);
    hub.detach(key(), 1).await;
    wait_released(&hub).await;
    assert!(opener.calls.lock().unwrap().is_empty());
    hub.shutdown_all().await;
}
