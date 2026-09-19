use super::*;
use coda_execution::TaskMeta;
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
    let id = coda_execution::TaskId::new();
    let own_id = id.clone();
    let tool = TaskKillTool::new(background.clone());
    let meta = coda_execution::TaskMeta {
        kind: coda_execution::TaskKind::Subagent {
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
            coda_execution::TaskExit::Killed
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
        background.read_result(&id).await.unwrap(),
        Some(coda_execution::TaskResult::Available {
            status: coda_execution::TaskStatus::Killed { .. },
            ..
        })
    ));
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
async fn task_pages_advance_only_when_the_checkpoint_commits() {
    use coda_core::output::{Channel, OutputData};
    let background = Arc::new(BackgroundTasks::temporary().unwrap());
    let id = background
        .spawn_with(meta("pages"), |ctx| async move {
            ctx.append_stdout("中🙂".repeat(4000).as_bytes())
                .await
                .unwrap();
            coda_execution::TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    background.wait_terminal(&id).await;
    let tool = TaskOutputTool::new(background.clone());
    let mut read = String::new();
    let mut done = false;
    for _ in 0..100 {
        let ctx = ToolCallContext::default();
        let OutputData::Page { body, .. } = tool
            .execute(
                TaskOutputToolParams {
                    id: id.to_string(),
                    byte_offset: None,
                },
                ctx.clone(),
            )
            .await
            .unwrap()
        else {
            panic!("expected page")
        };
        assert!(body.len() <= ctx.output_bytes);
        let OutputData::Page { body: repeated, .. } = tool
            .execute(
                TaskOutputToolParams {
                    id: id.to_string(),
                    byte_offset: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap()
        else {
            panic!("expected page")
        };
        assert_eq!(body, repeated, "an uncommitted read must be retryable");
        let receipts = ctx.take_reads();
        let stdout = receipts
            .iter()
            .find(|r| r.channel == Channel::Stdout)
            .unwrap();
        let heading = "\nstdout (new):\n";
        if stdout.end > stdout.start {
            let start = body.find(heading).unwrap() + heading.len();
            read.push_str(&body[start..][..(stdout.end - stdout.start) as usize]);
        }
        done = receipts.iter().all(|r| r.complete);
        background.commit_reads(&receipts).await;
        if done {
            break;
        }
    }
    assert!(done);
    assert_eq!(read, "中🙂".repeat(4000));
    assert_eq!(
        background.output_progress("", &id, Channel::Stdout).await,
        read.len() as u64
    );
    background.shutdown().await;
}
