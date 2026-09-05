use super::super::super::*;
use super::super::fixtures::{assistant, user_task};
use super::fixtures::*;
use crate::runtime::{MemoryStorage, SessionStorage, StoredResumePoint};
use crate::{AgentSpec, AgentTeam, ModelProfile, RunConfig};
use coda_process::TaskStatus;
use tokio::time::{Duration, timeout};
#[tokio::test]
async fn child_checkpoint_failure_finishes_scope_but_quarantines_until_abort_is_durable() {
    timeout(Duration::from_secs(10), async {
        let storage = FaultStorage::default();
        storage.fail_approval.store(true, std::sync::atomic::Ordering::SeqCst);
        storage.block_cleanup.store(true, std::sync::atomic::Ordering::SeqCst);
        let provider = BackgroundProvider { approval: true, ..Default::default() };
        let (runtime, background, _) = start_storage(provider.clone(), storage.clone()).await;
        runtime.send_message(user_task(&ThreadId::from("background-session".to_string()), "start")).await.unwrap();
        provider.child_started.notified().await;
        let id: coda_core::task::TaskId = background.summaries().borrow()[0].id.parse().unwrap();
        let independent = background.spawn_with(coda_process::TaskMeta::shell("unrelated".into(), "unrelated".into(), "coda".into()), |ctx| async move { ctx.cancelled().cancelled().await; coda_process::TaskExit::Killed }).await.unwrap();
        provider.child_release.notify_one();
        background.wait_terminal(&id).await;
        assert!(matches!(background.read(&id).await.unwrap().unwrap().status, TaskStatus::Failed { message, .. } if message.contains("injected child")));
        assert!(runtime.pending_approvals().is_empty());
        assert!(runtime.has_background_work(), "cleanup outage keeps members quarantined");
        assert!(background.read(&independent).await.unwrap().unwrap().status.is_running());
        let old_child = storage.inner.all_checkpoints().await.into_iter().find(|c| c.agent_name == "child").unwrap();
        let execution = old_child.active_execution.clone().unwrap();
        storage.block_cleanup.store(false, std::sync::atomic::Ordering::SeqCst);
        while runtime.has_background_work() { tokio::task::yield_now().await; }
        assert!(!background.has_pending_cleanup().await);
        let checkpoint = storage.load_checkpoint(&old_child.thread_id).await.unwrap().unwrap();
        assert!(checkpoint.active_execution.is_none());
        assert!(matches!(checkpoint.resume_point, StoredResumePoint::Generation));
        assert!(storage.save_execution_checkpoint(crate::execution::ExecutionIdentity { thread_id: old_child.thread_id.clone(), invocation_id: execution.invocation_id }, old_child).await.is_err(), "late writes cannot restore stale tool execution");
        background.kill(&independent).await.unwrap();
        runtime.request_exit().await;
        runtime.wait_for_exit(Some(Duration::from_secs(2))).await;
        background.shutdown().await;
    }).await.expect("a failed descendant must not keep its caller waiting");
}

#[tokio::test]
async fn ambiguous_notice_commit_retries_the_same_opening_and_wakes_root_once() {
    timeout(Duration::from_secs(5), async {
        let provider = BackgroundProvider::default();
        let storage = FaultStorage::default();
        let (runtime, background, mut events) =
            start_storage(provider.clone(), storage.clone()).await;
        runtime
            .send_message(user_task(
                &ThreadId::from("background-session".to_string()),
                "start",
            ))
            .await
            .unwrap();
        provider.child_started.notified().await;
        loop {
            let (_, _, _, event) = events.recv().await.unwrap();
            if matches!(event, AgentEvent::LLMEnd(ref answer) if answer.content == "root is free") {
                break;
            }
        }
        provider.child_release.notify_one();
        let id: coda_core::task::TaskId = background.summaries().borrow()[0].id.parse().unwrap();
        background.wait_terminal(&id).await;
        storage
            .lose_notice_reply
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            runtime
                .admit_background_notice("coda".into(), id.clone(), vec![], "result".into())
                .await
                .is_err()
        );
        assert!(
            runtime
                .admit_background_notice("coda".into(), id.clone(), vec![], "result".into())
                .await
                .unwrap()
        );
        assert!(
            !runtime
                .admit_background_notice("coda".into(), id, vec![], "duplicate".into())
                .await
                .unwrap()
        );
        loop {
            let (_, _, _, event) = events.recv().await.unwrap();
            if matches!(event, AgentEvent::LLMEnd(ref answer) if answer.content == "root is free") {
                break;
            }
        }
        assert_eq!(
            storage
                .inner
                .load_checkpoint("background-session")
                .await
                .unwrap()
                .unwrap()
                .messages
                .iter()
                .filter(|entry| matches!(entry.message, Message::TaskNotice(_)))
                .count(),
            1
        );
        runtime.request_exit().await;
        runtime.wait_for_exit(Some(Duration::from_secs(2))).await;
        background.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cold_open_cleans_background_approvals_before_bootstrap() {
    let storage = MemoryStorage::default();
    let root = "cold-background";
    let task = coda_core::task::TaskId::new();
    let mut answer = assistant();
    answer.tool_calls.push(ToolCall {
        id: "old-call".into(),
        name: "read_todos".into(),
        arguments: Some("{}".into()),
    });
    let turn = TurnId::from(MessageId::new());
    storage
        .save_checkpoint(
            "child".into(),
            crate::StoredCheckpoint {
                thread_id: "child".into(),
                agent_name: "child".into(),
                parent_thread_id: Some(root.into()),
                derivation_key: Some("child".into()),
                active_execution: Some(crate::execution::StoredExecution {
                    invocation_id: "old-execution".into(),
                    scope: crate::execution::ExecutionScope::Background {
                        task_id: task.clone(),
                    },
                    completion: crate::execution::CompletionTarget::BackgroundTask(task),
                    agent_path: vec!["coda".into(), "child".into()],
                }),
                messages: vec![crate::HistoryEntry::new(
                    turn,
                    Message::Assistant(answer.clone()),
                )],
                resume_point: StoredResumePoint::PendingApproval {
                    parent_message_id: answer.message_id,
                    pending_approval_calls: vec![crate::persist::StoredPreparedToolCall {
                        tool_call: answer.tool_calls[0].clone(),
                        metadata: None,
                    }],
                    pending_calls: vec![],
                },
                suspended_at: jiff::Timestamp::now(),
            },
        )
        .await
        .unwrap();
    storage
        .save_session_snapshot(
            root.into(),
            crate::StoredRuntimeSnapshot {
                active_threads: [("child".into(), "child".into())].into(),
                drained_envelopes: Default::default(),
                agent_drained_envelopes: Default::default(),
            },
        )
        .await
        .unwrap();
    let spec = |name: &str, subagents: Vec<String>| AgentSpec {
        name: name.into(),
        description: String::new(),
        system_prompt: name.into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents,
    };
    let team = AgentTeam::new(
        spec("coda", vec!["child".into()]),
        vec![spec("child", vec![])],
    )
    .unwrap();
    let session = crate::Session::builder()
        .storage(storage.clone())
        .team(&team, ".")
        .session_id(root)
        .background(None)
        .run_config(RunConfig {
            default_model: ModelProfile {
                provider: BackgroundProvider::default(),
                model: "fake".into(),
                label: "fake".into(),
                temperature: None,
                max_completion_tokens: None,
                reasoning_effort: None,
                auto_compact_threshold_tokens: u32::MAX,
            },
            agent_models: HashMap::new(),
            tool_approval: ToolApprovalMode::Auto,
            approval_timeout: None,
        })
        .open()
        .await
        .unwrap();
    assert!(!session.has_resuming_agents());
    assert!(session.pending_approvals().is_empty());
    let cleaned = storage.load_checkpoint("child").await.unwrap().unwrap();
    assert!(cleaned.active_execution.is_none());
    assert!(
        matches!(cleaned.messages.last().unwrap().message, Message::Tool(ref tool) if matches!(tool.outcome, ToolCallOutcome::Aborted))
    );
    session.shutdown(crate::Shutdown::abort()).await;
}
