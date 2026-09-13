use super::*;
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
            ..
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
        .open(
            reg.backend
                .archive
                .open(&id)
                .await
                .unwrap()
                .unwrap()
                .files()
                .snapshot()
                .reference
                .unwrap()
                .channels
                .iter()
                .find(|c| c.channel == coda_core::output::Channel::Stdout)
                .unwrap()
                .path
                .clone(),
        )
        .unwrap()
        .set_len(0)
        .unwrap();
    assert!(reg.read_result(&id).await.is_err());
    reg.shutdown().await;
}

#[tokio::test]
async fn concurrent_pages_repeat_until_commit_and_consumers_progress_independently() {
    use coda_core::output::Channel;
    let reg = BackgroundTasks::temporary().unwrap();
    let id = reg
        .spawn_with(meta(), |ctx| async move {
            ctx.append_stdout("页内容🙂".repeat(6000).as_bytes())
                .await
                .unwrap();
            TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    reg.wait_terminal(&id).await;
    let (first, concurrent) = tokio::join!(
        reg.read_page(&id, "root", [0; 3], None, 8192),
        reg.read_page(&id, "root", [0; 3], None, 8192),
    );
    let first = first.unwrap().unwrap();
    assert_eq!(first.body, concurrent.unwrap().unwrap().body);
    assert!(first.body.len() <= 8192);
    assert!(!first.body.contains('�'));
    assert!(!first.complete);
    assert_eq!(first.references.len(), 1);
    assert_eq!(reg.output_progress("root", &id, Channel::Stdout).await, 0);
    reg.commit_reads(&first.receipts).await;
    let next = first.receipts[0].end;
    assert!(next > 0);
    assert_eq!(
        reg.output_progress("root", &id, Channel::Stdout).await,
        next
    );
    assert_eq!(reg.output_progress("child", &id, Channel::Stdout).await, 0);
    let child = reg
        .read_page(&id, "child", [0; 3], None, 8192)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.body, first.body);
    assert!(!reg.take_notices().await.is_empty());
    reg.shutdown().await;
}
