use super::*;
use coda_process::TaskMeta;
use tokio::process::Command;

fn bash(command: &str) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(command);
    cmd
}

fn meta(command: &str) -> TaskMeta {
    TaskMeta::shell(command.into(), "test task".into(), "coda".into())
}

#[tokio::test]
async fn a_subagent_can_request_its_own_stop_without_waiting_for_itself() {
    let background = Arc::new(BackgroundTasks::temporary().unwrap());
    let id = coda_process::TaskId::new();
    let own_id = id.clone();
    let tool = TaskKillTool::new(background.clone());
    let meta = coda_process::TaskMeta {
        kind: coda_process::TaskKind::Subagent {
            agent_name: "worker".into(),
        },
        description: "self stop".into(),
        parent_task_id: None,
        origin: Default::default(),
    };
    background
        .spawn_identified(id.clone(), meta, move |_| async move {
            let mut ctx = ToolCallContext::default();
            ctx.background_task = Some(own_id.clone());
            tool.execute(
                TaskKillToolParams {
                    id: own_id.to_string(),
                },
                ctx,
            )
            .await
            .unwrap();
            coda_process::TaskExit::Killed
        })
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        background.wait_terminal(&id),
    )
    .await
    .expect("self stop must not wait on its own monitor");
    assert!(matches!(
        background.read(&id).await.unwrap().unwrap().status,
        coda_process::TaskStatus::Killed { .. }
    ));
}

#[tokio::test]
async fn task_output_reads_incrementally_and_reports_expiry() {
    let background = Arc::new(BackgroundTasks::temporary().unwrap());
    let id = background
        .spawn(bash("echo first; sleep 39.01"), meta("stream"))
        .await
        .unwrap();
    let tool = TaskOutputTool::new(background.clone());

    // First read eventually sees "first"; the next read must not repeat it.
    let progress = ToolCallContext::default();
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let out = tool
                .execute(
                    TaskOutputToolParams { id: id.to_string() },
                    progress.clone(),
                )
                .await
                .unwrap();
            if out.contains("first") {
                break out;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("output never arrived");
    assert!(out.contains("status: running"), "unexpected: {out}");
    assert!(progress.take_task_result().is_none());

    let again = tool
        .execute(
            TaskOutputToolParams { id: id.to_string() },
            ToolCallContext::default(),
        )
        .await
        .unwrap();
    assert!(
        again.contains("(no new output)"),
        "second read repeated output: {again}"
    );

    let missing = tool
        .execute(
            TaskOutputToolParams {
                id: "bg_00000000000000000000000000000000".into(),
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap();
    assert!(
        missing.contains("Unknown or expired task id"),
        "unexpected: {missing}"
    );
    background.shutdown().await;
}

#[tokio::test]
async fn task_kill_terminates_and_is_idempotent() {
    let background = Arc::new(BackgroundTasks::temporary().unwrap());
    let id = background
        .spawn(bash("sleep 39.21"), meta("victim"))
        .await
        .unwrap();
    let tool = TaskKillTool::new(background.clone());

    let out = tool
        .execute(
            TaskKillToolParams { id: id.to_string() },
            ToolCallContext::default(),
        )
        .await
        .unwrap();
    assert!(out.contains("killed"), "unexpected: {out}");

    // Idempotent: reports the settled status instead of failing.
    let again = tool
        .execute(
            TaskKillToolParams { id: id.to_string() },
            ToolCallContext::default(),
        )
        .await
        .unwrap();
    assert!(again.contains("killed"), "unexpected: {again}");

    let missing = tool
        .execute(
            TaskKillToolParams {
                id: "bg_00000000000000000000000000000000".into(),
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap();
    assert!(
        missing.contains("Unknown or expired task id"),
        "unexpected: {missing}"
    );
    background.shutdown().await;
}

#[tokio::test]
async fn task_output_never_records_a_terminal_read_after_paginated_loss() {
    let background = Arc::new(BackgroundTasks::temporary().unwrap());
    let id = background
        .spawn_with(meta("overwritten output"), |ctx| async move {
            let bytes = vec![b'x'; coda_process::DEFAULT_STREAM_CAPACITY as usize + 7];
            ctx.append_stdout(&bytes).await.unwrap();
            coda_process::TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    background.wait_terminal(&id).await;
    let tool = TaskOutputTool::new(background.clone());
    loop {
        let ctx = ToolCallContext::default();
        let out = tool
            .execute(TaskOutputToolParams { id: id.to_string() }, ctx.clone())
            .await
            .unwrap();
        assert!(
            ctx.take_task_result().is_none(),
            "no page may acknowledge a result with missing output"
        );
        if out.contains("output fully consumed") {
            break;
        }
    }
    assert!(
        background
            .take_notices()
            .await
            .iter()
            .any(|notice| matches!(
                notice,
                coda_process::TaskNotice::Task { id: notice_id, .. } if notice_id == &id
            ))
    );
    background.shutdown().await;
}

#[tokio::test]
async fn task_output_only_records_a_complete_terminal_read() {
    let background = Arc::new(BackgroundTasks::temporary().unwrap());
    let id = background
        .spawn_with(meta("large output"), |ctx| async move {
            ctx.append_stdout(&vec![b'x'; 200 * 1024]).await.unwrap();
            coda_process::TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    background.wait_terminal(&id).await;
    let tool = TaskOutputTool::new(background.clone());
    let first = ToolCallContext::default();
    tool.execute(TaskOutputToolParams { id: id.to_string() }, first.clone())
        .await
        .unwrap();
    assert!(
        first.take_task_result().is_none(),
        "a partial page must not suppress the notice"
    );
    let last = ToolCallContext::default();
    tool.execute(TaskOutputToolParams { id: id.to_string() }, last.clone())
        .await
        .unwrap();
    assert_eq!(last.take_task_result(), Some(id.clone()));
    let consumed = ToolCallContext::default();
    tool.execute(
        TaskOutputToolParams { id: id.to_string() },
        consumed.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        consumed.take_task_result(),
        Some(id.clone()),
        "terminal status remains observable after lossless output reclamation"
    );
    assert_eq!(
        background.take_notices().await.len(),
        1,
        "reading alone does not acknowledge delivery before checkpoint"
    );
    background.shutdown().await;
}
