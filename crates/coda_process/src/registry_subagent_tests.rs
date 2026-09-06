use super::*;
use coda_core::task::ScopeMember;
use tokio::time::{Duration, timeout};

fn subagent() -> TaskMeta {
    TaskMeta {
        kind: TaskKind::Subagent {
            agent_name: "worker".into(),
        },
        description: "work".into(),
        parent_task_id: None,
        origin: TaskOrigin::default(),
    }
}

#[tokio::test]
async fn durable_pending_results_survive_reopen_and_are_read_repeatedly() {
    let temp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(temp.path()).unwrap();
    let registry = BackgroundTasks::session_backed(root.clone()).await.unwrap();
    let answer = "完整答复".repeat(4000);
    let expected = answer.clone();
    let id = registry
        .spawn_identified(TaskId::new(), subagent(), |_| async move {
            TaskExit::Completed { answer }
        })
        .await
        .unwrap();
    registry.wait_terminal(&id).await;
    assert_eq!(registry.take_notices().await.len(), 1);
    assert_eq!(registry.read(&id).await.unwrap().unwrap().stdout, expected);
    registry.shutdown().await;
    drop(registry);
    let registry = BackgroundTasks::session_backed(root).await.unwrap();
    assert_eq!(registry.take_notices().await.len(), 1);
    assert_eq!(registry.read(&id).await.unwrap().unwrap().stdout, expected);
    assert_eq!(registry.read(&id).await.unwrap().unwrap().stdout, expected);
    registry.acknowledge_notice(&id).await.unwrap();
    assert!(registry.take_notices().await.is_empty());
    assert!(registry.backend.quota.retained_contains(&id));
}

#[tokio::test]
async fn restart_interrupts_an_uncommitted_result_and_preserves_cleanup_members() {
    let temp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(temp.path()).unwrap();
    let archive = TaskArchive::new(root.clone());
    let id = TaskId::new();
    let record = archive.create_unreserved(&id, &subagent()).await.unwrap();
    let member = ScopeMember {
        thread_id: "child".into(),
        invocation_id: "invocation".into(),
    };
    {
        let mut guard = record.lock_commit().await;
        let mut candidate = guard.current().clone();
        candidate.scope_members = vec![member.clone()];
        guard.commit(candidate).await.unwrap();
    }
    record
        .write_result("not yet committed".into())
        .await
        .unwrap();
    drop(record);
    drop(archive);
    let registry = BackgroundTasks::session_backed(root).await.unwrap();
    assert!(matches!(
        registry.read(&id).await.unwrap().unwrap().status,
        TaskStatus::Interrupted { .. }
    ));
    assert_eq!(
        registry.recovered_scopes().await,
        vec![(id.clone(), vec![member.clone()])]
    );
    assert!(registry.has_pending_cleanup().await);
    assert_eq!(registry.take_notices().await.len(), 1);
    registry
        .record_scope(&id, vec![member], false)
        .await
        .unwrap();
    assert!(!registry.has_pending_cleanup().await);
    assert!(registry.recovered_scopes().await.is_empty());
}

#[tokio::test]
async fn completed_parent_retains_stop_control_over_its_shell_children() {
    timeout(Duration::from_secs(5), async {
        let registry = BackgroundTasks::temporary().unwrap();
        let parent = registry
            .spawn_identified(TaskId::new(), subagent(), |_| async {
                TaskExit::Completed {
                    answer: "done".into(),
                }
            })
            .await
            .unwrap();
        registry.wait_terminal(&parent).await;
        let mut shell = TaskMeta::shell("child".into(), "child".into(), "worker".into());
        shell.parent_task_id = Some(parent.clone());
        let child = registry
            .spawn_with(shell.clone(), |ctx| async move {
                ctx.cancelled().cancelled().await;
                TaskExit::Killed
            })
            .await
            .unwrap();
        assert!(
            registry
                .summaries()
                .borrow()
                .iter()
                .find(|task| task.id == parent.as_str())
                .unwrap()
                .subtree_active
        );
        registry.kill(&parent).await.unwrap();
        assert!(matches!(
            registry.read(&parent).await.unwrap().unwrap().status,
            TaskStatus::Completed { .. }
        ));
        assert!(matches!(
            registry.read(&child).await.unwrap().unwrap().status,
            TaskStatus::Killed { .. }
        ));
        assert!(
            registry
                .spawn_with(shell, |_| async { TaskExit::Exited { code: Some(0) } })
                .await
                .is_err()
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn pending_notice_capacity_rejects_new_subagents_without_dropping_results() {
    let registry = BackgroundTasks::temporary().unwrap();
    for _ in 0..64 {
        let id = registry
            .spawn_identified(TaskId::new(), subagent(), |_| async {
                TaskExit::Completed {
                    answer: String::new(),
                }
            })
            .await
            .unwrap();
        registry.wait_terminal(&id).await;
    }
    assert_eq!(registry.take_notices().await.len(), 64);
    assert!(
        registry
            .spawn_identified(TaskId::new(), subagent(), |_| async {
                TaskExit::Completed {
                    answer: "too late".into(),
                }
            })
            .await
            .is_err()
    );
    assert_eq!(registry.take_notices().await.len(), 64);
}
