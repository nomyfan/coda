use super::*;
use crate::DEFAULT_STREAM_CAPACITY;
use tokio::sync::Notify;

fn meta() -> TaskMeta {
    TaskMeta::shell("command".into(), "panel result".into(), "coda".into())
}

#[tokio::test]
async fn panel_reads_preserve_model_cursors_and_completion_notice() {
    let reg = BackgroundTasks::temporary().unwrap();
    assert!(reg.read_result(&TaskId::new()).await.unwrap().is_none());
    let ready = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let task_ready = ready.clone();
    let task_finish = finish.clone();
    let id = reg
        .spawn_with(meta(), move |ctx| async move {
            ctx.append_stdout(b"first").await.unwrap();
            task_ready.notify_one();
            task_finish.notified().await;
            ctx.append_stdout(b" second").await.unwrap();
            ctx.append_stderr(b"warning").await.unwrap();
            TaskExit::Exited { code: Some(1) }
        })
        .await
        .unwrap();
    ready.notified().await;
    assert!(matches!(
        reg.read_result(&id).await.unwrap(),
        Some(TaskResult::Pending { .. })
    ));
    assert_eq!(reg.read(&id).await.unwrap().unwrap().stdout, "first");
    finish.notify_one();
    reg.wait_terminal(&id).await;
    for _ in 0..2 {
        let Some(TaskResult::Available {
            status,
            output:
                TaskResultOutput::Shell {
                    stdout,
                    stderr,
                    stdout_overwritten,
                    stderr_overwritten,
                },
        }) = reg.read_result(&id).await.unwrap()
        else {
            panic!("expected shell result")
        };
        assert!(matches!(status, TaskStatus::Exited { code: Some(1), .. }));
        assert_eq!(stdout, "first second");
        assert_eq!(stderr, "warning");
        assert_eq!((stdout_overwritten, stderr_overwritten), (0, 0));
    }
    let read = reg.read(&id).await.unwrap().unwrap();
    assert_eq!(read.stdout, " second");
    assert_eq!(read.stderr, "warning");
    assert!(read.complete);
    assert!(matches!(
        reg.read_result(&id).await.unwrap(),
        Some(TaskResult::Available { .. })
    ));
    assert!(reg.take_notices().await.iter().any(|notice| matches!(
        notice, TaskNotice::Task { id: notice_id, .. } if notice_id == &id
    )));
    reg.shutdown().await;
}

#[tokio::test]
async fn shell_panel_snapshot_survives_reopen_and_reports_overwrite() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let reg = BackgroundTasks::session_backed(root.clone()).await.unwrap();
    let id = reg
        .spawn_with(meta(), |ctx| async move {
            ctx.append_stdout(&vec![b'x'; DEFAULT_STREAM_CAPACITY as usize + 7])
                .await
                .unwrap();
            ctx.append_stderr(&[0xff]).await.unwrap();
            TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    reg.wait_terminal(&id).await;
    reg.shutdown().await;
    drop(reg);
    let reg = BackgroundTasks::session_backed(root).await.unwrap();
    let Some(TaskResult::Available {
        output:
            TaskResultOutput::Shell {
                stdout,
                stderr,
                stdout_overwritten,
                stderr_overwritten,
            },
        ..
    }) = reg.read_result(&id).await.unwrap()
    else {
        panic!("expected shell result")
    };
    assert_eq!(stdout, "x".repeat(DEFAULT_STREAM_CAPACITY as usize));
    assert_eq!(stdout_overwritten, 7);
    assert_eq!(stderr, "\u{fffd}");
    assert_eq!(stderr_overwritten, 0);
    // UI reads must not absorb the loss that the model still needs to see.
    assert_eq!(reg.read(&id).await.unwrap().unwrap().stdout_lost, 7);
    reg.shutdown().await;
}

#[tokio::test]
async fn quota_evicted_panel_results_are_expired_even_if_model_consumed_them() {
    for consumed in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
        let archive = Arc::new(TaskArchive::new(root));
        let quota = SessionQuota::from_inventory(
            &ArchiveInventory::default(),
            2 * DEFAULT_STREAM_CAPACITY,
            archive.clone(),
        );
        let reg = BackgroundTasks::new(Arc::new(Backend {
            archive,
            quota,
            temp: None,
        }));
        let id = reg
            .spawn_with(meta(), |ctx| async move {
                ctx.append_stdout(b"output").await.unwrap();
                TaskExit::Exited { code: Some(0) }
            })
            .await
            .unwrap();
        reg.wait_terminal(&id).await;
        if consumed {
            reg.read(&id).await.unwrap();
        }
        let next = reg
            .spawn_with(meta(), |_| async { TaskExit::Exited { code: Some(0) } })
            .await
            .unwrap();
        reg.wait_terminal(&next).await;
        assert!(matches!(
            reg.read_result(&id).await.unwrap(),
            Some(TaskResult::Expired { .. })
        ));
        reg.shutdown().await;
    }
}

#[tokio::test]
async fn panel_subagent_reads_leave_delivery_pending() {
    let reg = BackgroundTasks::temporary().unwrap();
    let mut meta = meta();
    meta.kind = TaskKind::Subagent {
        agent_name: "worker".into(),
    };
    let id = reg
        .spawn_identified(TaskId::new(), meta, |_| async {
            TaskExit::Completed {
                answer: "**done**".into(),
            }
        })
        .await
        .unwrap();
    reg.wait_terminal(&id).await;
    for _ in 0..2 {
        let Some(TaskResult::Available {
            output: TaskResultOutput::Subagent { answer },
            ..
        }) = reg.read_result(&id).await.unwrap()
        else {
            panic!("expected subagent answer")
        };
        assert_eq!(answer, "**done**");
        assert_eq!(reg.take_notices().await.len(), 1);
    }
    reg.shutdown().await;
}

#[tokio::test]
async fn missing_retained_bytes_are_a_read_error_not_empty_output() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let reg = BackgroundTasks::session_backed(root).await.unwrap();
    let id = reg
        .spawn_with(meta(), |ctx| async move {
            ctx.append_stdout(b"expected output").await.unwrap();
            TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    reg.wait_terminal(&id).await;
    std::fs::OpenOptions::new()
        .write(true)
        .open(tmp.path().join(id.as_str()).join("stdout.ring"))
        .unwrap()
        .set_len(0)
        .unwrap();
    assert!(reg.read_result(&id).await.is_err());
    reg.shutdown().await;
}
