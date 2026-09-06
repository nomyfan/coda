use super::*;
use crate::{StoredCheckpoint, StoredRuntimeSnapshot};
use coda_core::llm::MessageId;
use coda_execution::BackgroundTasks;

#[tokio::test]
async fn old_group_retirement_and_abort_preserve_a_new_execution() {
    let storage = MemoryStorage::default();
    let mut runtime = ProcessRuntime::new(storage.clone(), "session".into());
    let background = Arc::new(BackgroundTasks::temporary().unwrap());
    runtime.background = Some(background.clone());
    let pid = ProcessId::new();
    let origin = MessageOrigin {
        message_id: MessageId::new(),
        call_id: "old".into(),
    };
    let old_group = TaskId::for_call("session", pid.as_ref(), &origin);
    background
        .spawn_identified(
            old_group.clone(),
            TaskMeta {
                kind: TaskKind::Subagent {
                    agent_name: "worker".into(),
                },
                description: "completed old work".into(),
                parent_task_id: None,
                origin: TaskOrigin::default(),
            },
            |_| async {
                TaskExit::Completed {
                    answer: "done".into(),
                }
            },
        )
        .await
        .unwrap();
    background.wait_terminal(&old_group).await;
    let current = StoredExecution {
        invocation_id: "new-execution".into(),
        scope: ProcessGroupId::Foreground {
            turn_id: TurnId::from(MessageId::new()),
        },
        completion: CompletionTarget::RootTurn,
        agent_path: vec!["worker".into()],
    };
    let member = ScopeMember {
        pid: pid.0.clone(),
        invocation_id: "old-execution".into(),
    };
    let cancel = CancellationToken::new();
    runtime.executions.lock().unwrap().processes.insert(
        pid.0.clone(),
        LiveExecution {
            stored: current.clone(),
            cancel: cancel.clone(),
        },
    );
    let checkpoint = StoredCheckpoint {
        pid: pid.0.clone(),
        agent_name: "worker".into(),
        parent_pid: Some("session".into()),
        derivation_key: Some("worker".into()),
        active_execution: Some(current.clone()),
        messages: vec![],
        resume_point: StoredResumePoint::Generation,
        suspended_at: Default::default(),
    };
    storage
        .save_checkpoint(pid.0.clone(), checkpoint)
        .await
        .unwrap();
    let snapshot = StoredRuntimeSnapshot {
        active_processes: [(pid.0.clone(), "worker".into())].into(),
        drained_envelopes: HashMap::new(),
        agent_drained_envelopes: HashMap::new(),
    };
    storage
        .save_session_snapshot("session".into(), snapshot.clone())
        .await
        .unwrap();
    *runtime.snapshot.lock().await = snapshot.into();

    // Retiring a group with a historical membership must not retire the newer process driver.
    runtime.executions.lock().unwrap().background.insert(
        old_group.clone(),
        BackgroundGroup {
            members: vec![member.clone()],
            completion: None,
            closed: true,
            stopping: false,
            reason: None,
            stopped: tokio::sync::watch::channel(false).0,
        },
    );
    assert!(runtime.retire_background_group(&old_group).await);
    assert_eq!(
        runtime.execution(&pid).unwrap().invocation_id,
        current.invocation_id
    );
    assert!(!cancel.is_cancelled());

    // A later abort also fences the old invocation without deleting new checkpoint/snapshot state.
    runtime.executions.lock().unwrap().background.insert(
        old_group.clone(),
        BackgroundGroup {
            members: vec![member.clone()],
            completion: None,
            closed: true,
            stopping: false,
            reason: None,
            stopped: tokio::sync::watch::channel(false).0,
        },
    );
    runtime
        .stop_background_group(&old_group, Some("old failure".into()))
        .await;
    timeout(Duration::from_secs(2), async {
        while runtime.has_background_work() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!cancel.is_cancelled());
    assert_eq!(
        runtime.execution(&pid).unwrap().invocation_id,
        current.invocation_id
    );
    assert!(
        runtime
            .snapshot
            .lock()
            .await
            .active_processes
            .contains_key(pid.as_ref())
    );
    let checkpoint = storage
        .load_checkpoint(pid.as_ref())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        checkpoint.active_execution.as_ref().unwrap().invocation_id,
        current.invocation_id
    );
    assert!(
        storage
            .load_session_snapshot("session")
            .await
            .unwrap()
            .unwrap()
            .active_processes
            .contains_key(pid.as_ref())
    );
    assert!(
        storage
            .save_execution_checkpoint(
                ExecutionIdentity {
                    pid: pid.0.clone(),
                    invocation_id: member.invocation_id,
                },
                checkpoint
            )
            .await
            .is_err()
    );
    runtime.request_exit().await;
    runtime.wait_for_exit(Some(Duration::from_secs(2))).await;
    background.shutdown().await;
}
