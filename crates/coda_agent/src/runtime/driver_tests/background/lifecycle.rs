use super::super::super::*;
use super::super::fixtures::user_task;
use super::fixtures::*;
use crate::runtime::{SessionStorage, StoredResumePoint};
use coda_process::TaskStatus;
use tokio::time::{Duration, timeout};
#[tokio::test]
async fn background_tree_outlives_root_turn_and_delivers_complete_result_once() {
    timeout(Duration::from_secs(10), async {
        let provider = BackgroundProvider::default();
        let (runtime, storage, background, mut events) = start(provider.clone()).await;
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
        let summary = background.summaries().borrow()[0].clone();
        assert!(summary.status.is_running());
        runtime.request_abort().await;
        assert!(background.summaries().borrow()[0].status.is_running());
        provider.child_release.notify_one();
        let id = summary.id.parse().unwrap();
        background.wait_terminal(&id).await;
        assert!(matches!(
            background.read(&id).await.unwrap().unwrap().status,
            TaskStatus::Completed { .. }
        ));
        let answer = background.read(&id).await.unwrap().unwrap().stdout;
        assert_eq!(answer, "complete final answer".repeat(2000));
        assert_eq!(background.read(&id).await.unwrap().unwrap().stdout, answer);
        let notices = background.take_notices().await;
        assert_eq!(notices.len(), 1);
        assert!(
            runtime
                .admit_background_notice(
                    "coda".into(),
                    id.clone(),
                    vec![notices[0].outcome()],
                    answer
                )
                .await
                .unwrap()
        );
        assert!(
            !runtime
                .admit_background_notice("coda".into(), id.clone(), vec![], "duplicate".into())
                .await
                .unwrap()
        );
        background.acknowledge_notice(&id).await.unwrap();
        assert!(background.take_notices().await.is_empty());
        loop {
            let (_, _, _, event) = events.recv().await.unwrap();
            if matches!(event, AgentEvent::LLMEnd(ref answer) if answer.content == "root is free") {
                break;
            }
        }
        let root = storage
            .load_checkpoint("background-session")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            root.messages
                .iter()
                .filter(|entry| matches!(entry.message, Message::TaskNotice(_)))
                .count(),
            1
        );
        let child = storage
            .all_checkpoints()
            .await
            .into_iter()
            .find(|c| c.agent_name == "child")
            .unwrap();
        assert!(child.active_execution.is_none());
        runtime.stop_background().await;
        runtime.request_exit().await;
        assert!(runtime.wait_for_exit(Some(Duration::from_secs(2))).await);
        background.shutdown().await;
    })
    .await
    .expect("background tree must finish independently of its root turn");
}

#[tokio::test]
async fn kill_cancels_synchronous_child_without_waiting_for_its_reply() {
    timeout(Duration::from_secs(10), async {
        let provider = BackgroundProvider::default();
        let (runtime, storage, background, _) = start(provider.clone()).await;
        runtime
            .send_message(user_task(
                &ThreadId::from("background-session".to_string()),
                "start",
            ))
            .await
            .unwrap();
        provider.child_started.notified().await;
        let id = background.summaries().borrow()[0].id.parse().unwrap();
        let status = background.kill(&id).await.unwrap().unwrap();
        assert!(matches!(status, TaskStatus::Killed { .. }));
        while runtime.has_background_work() {
            tokio::task::yield_now().await;
        }
        assert!(runtime.pending_approvals().is_empty());
        for checkpoint in storage
            .all_checkpoints()
            .await
            .into_iter()
            .filter(|c| c.agent_name != "coda")
        {
            assert!(checkpoint.active_execution.is_none());
            assert!(matches!(
                checkpoint.resume_point,
                StoredResumePoint::Generation
            ));
        }
        runtime.request_exit().await;
        runtime.wait_for_exit(Some(Duration::from_secs(2))).await;
        background.shutdown().await;
    })
    .await
    .expect("scope cancellation must release Reply waits");
}

#[tokio::test]
async fn non_root_background_parameter_is_rejected_without_starting_the_child() {
    timeout(Duration::from_secs(5), async {
        let (runtime, storage, background, mut events) = start(BackgroundProvider { nested_background: true, ..Default::default() }).await;
        runtime.send_message(user_task(&ThreadId::from("background-session".to_string()), "start")).await.unwrap();
        loop { let (_, _, _, event) = events.recv().await.unwrap(); if matches!(event, AgentEvent::LLMEnd(ref a) if a.content == "root is free") { break; } }
        let id = background.summaries().borrow()[0].id.parse().unwrap();
        background.wait_terminal(&id).await;
        let checkpoints = storage.all_checkpoints().await;
        assert!(!checkpoints.iter().any(|checkpoint| checkpoint.agent_name == "child"));
        assert!(checkpoints.iter().find(|checkpoint| checkpoint.agent_name == "worker").unwrap().messages.iter().any(|entry| matches!(&entry.message, Message::Tool(tool) if matches!(&tool.output, ToolOutput::Err(message) if message.contains("Only the root")))));
        assert_eq!(background.summaries().borrow().len(), 1);
        runtime.request_exit().await;
        runtime.wait_for_exit(Some(Duration::from_secs(2))).await;
        background.shutdown().await;
    }).await.unwrap();
}

#[tokio::test]
async fn stateful_calls_are_busy_across_foreground_and_background_dispatch() {
    timeout(Duration::from_secs(5), async {
        let provider = BackgroundProvider::default();
        let (runtime, _, background, mut events) = start(provider.clone()).await;
        let root = ThreadId::from("background-session".to_string());
        runtime
            .send_message(user_task(&root, "start"))
            .await
            .unwrap();
        provider.child_started.notified().await;
        loop {
            let (_, _, _, event) = events.recv().await.unwrap();
            if matches!(event, AgentEvent::LLMEnd(ref a) if a.content == "root is free") {
                break;
            }
        }
        let origin = MessageOrigin {
            message_id: MessageId::new(),
            call_id: "second".into(),
        };
        let envelope = Envelope::with_id(|id| Envelope {
            id,
            from: Sender::Agent {
                name: "coda".into(),
                thread_id: root.clone(),
            },
            to: Receiver {
                name: "worker".into(),
                thread_id: ThreadId::from_uuid5(&root, "worker"),
            },
            reply_to: None,
            body: EnvelopeBody::ToolCall {
                call_id: origin.call_id.clone(),
                parent_message_id: origin.message_id,
                derivation_key: "worker".into(),
                turn_id: TurnId::from(MessageId::new()),
                task: "second call".into(),
            },
        });
        assert!(matches!(
            runtime.send_message(envelope.clone()).await,
            Err(crate::runtime::SendCommandError::ThreadBusy)
        ));
        assert!(
            runtime
                .dispatch_background(envelope.clone(), origin.clone(), root.clone())
                .await
                .unwrap_err()
                .contains("busy")
        );
        let non_root = ThreadId::from_uuid5(&root, "coda");
        assert!(
            runtime
                .dispatch_background(envelope, origin, non_root)
                .await
                .unwrap_err()
                .contains("root thread")
        );
        assert_eq!(background.summaries().borrow().len(), 1);
        runtime
            .send_message(user_task(&root, "root can continue"))
            .await
            .unwrap();
        runtime.stop_background().await;
        runtime.request_exit().await;
        runtime.wait_for_exit(Some(Duration::from_secs(2))).await;
        background.shutdown().await;
    })
    .await
    .unwrap();
}
