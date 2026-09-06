use super::super::super::*;
use super::super::fixtures::user_task;
use super::fixtures::*;
use coda_process::TaskStatus;
use tokio::time::{Duration, timeout};
#[tokio::test]
async fn background_approval_blocks_new_input_and_cancellation_revokes_only_its_batch() {
    timeout(Duration::from_secs(5), async {
        let provider = BackgroundProvider {
            approval: true,
            ..Default::default()
        };
        let (runtime, _, background, mut events) = start(provider.clone()).await;
        let root = ThreadId::from("background-session".to_string());
        runtime
            .send_message(user_task(&root, "start"))
            .await
            .unwrap();
        provider.child_started.notified().await;
        provider.child_release.notify_one();
        let mut root_finished = false;
        let mut pending = None;
        while !root_finished || pending.is_none() {
            let (_, _, _, event) = events.recv().await.unwrap();
            match event {
                AgentEvent::LLMEnd(answer) if answer.content == "root is free" => {
                    root_finished = true
                }
                AgentEvent::Suspended(approval) => pending = Some(approval),
                _ => {}
            }
        }
        let approval = pending.unwrap();
        assert_eq!(approval.agent_path, vec!["coda", "worker", "child"]);
        let id = approval.task_id.clone().unwrap();
        assert!(matches!(
            background.read(&id).await.unwrap().unwrap().status,
            TaskStatus::WaitingApproval
        ));
        assert!(!runtime.root_turn_active());
        assert!(matches!(
            runtime.send_message(user_task(&root, "blocked")).await,
            Err(crate::runtime::SendCommandError::PendingApprovals)
        ));
        background.kill(&id).await.unwrap();
        assert!(runtime.pending_approvals().is_empty());
        let stale = runtime
            .send_message(Envelope::with_id(|id| Envelope {
                id,
                from: Sender::User,
                to: Receiver {
                    name: approval.agent_name,
                    thread_id: ThreadId::from(approval.thread_id),
                },
                reply_to: None,
                body: EnvelopeBody::Resume(crate::ResumeDecision {
                    parent_message_id: approval.parent_message_id,
                    resolutions: vec![(approval.calls[0].id.clone(), ToolCallResolution::Execute)],
                }),
            }))
            .await;
        assert!(matches!(
            stale,
            Err(crate::runtime::SendCommandError::StaleApproval)
        ));
        runtime.request_exit().await;
        runtime.wait_for_exit(Some(Duration::from_secs(2))).await;
        background.shutdown().await;
    })
    .await
    .unwrap();
}
