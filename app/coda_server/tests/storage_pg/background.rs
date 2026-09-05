use super::*;
use coda_agent::execution::{
    CompletionTarget, ExecutionIdentity, ExecutionScope, ScopeAbort, StoredExecution,
};
use coda_core::task::{ScopeMember, TaskId};

fn execution(task: &TaskId, invocation: &str) -> StoredExecution {
    StoredExecution {
        invocation_id: invocation.into(),
        scope: ExecutionScope::Background {
            task_id: task.clone(),
        },
        completion: CompletionTarget::BackgroundTask(task.clone()),
        agent_path: vec!["coda".into(), "worker".into()],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn abort_transaction_cleans_calls_and_fences_late_checkpoints_and_snapshots() {
    let pool = pool().await;
    let workspace = workspace_id("scope_abort");
    seed_session(&pool, &workspace, "root").await;
    let storage = PgSessionStorage::new(pool, &workspace, "root");
    let task = TaskId::new();
    let turn = TurnId::from(MessageId::new());
    let Message::Assistant(mut assistant) = assistant("needs approval") else {
        unreachable!()
    };
    assistant.tool_calls.push(ToolCall {
        id: "call".into(),
        name: "shell".into(),
        arguments: Some("{}".into()),
    });
    let mut child = checkpoint(
        "child",
        vec![entry(turn, Message::Assistant(assistant.clone()))],
    );
    child.agent_name = "worker".into();
    child.parent_thread_id = Some("root".into());
    child.derivation_key = Some("child".into());
    child.active_execution = Some(execution(&task, "child-invocation"));
    child.resume_point = StoredResumePoint::PendingApproval {
        parent_message_id: assistant.message_id,
        pending_approval_calls: vec![StoredPreparedToolCall {
            tool_call: assistant.tool_calls[0].clone(),
            metadata: None,
        }],
        pending_calls: vec![],
    };
    let identity = ExecutionIdentity {
        thread_id: "child".into(),
        invocation_id: "child-invocation".into(),
    };
    storage
        .save_execution_checkpoint(identity.clone(), child.clone())
        .await
        .unwrap();
    let mut unrelated = checkpoint("unrelated", vec![]);
    unrelated.agent_name = "worker".into();
    unrelated.active_execution = Some(execution(&TaskId::new(), "unrelated-invocation"));
    storage
        .save_checkpoint("unrelated".into(), unrelated)
        .await
        .unwrap();
    let mut queued = queued_task("child", "must never replay");
    queued.id = identity.invocation_id.clone();
    let snapshot = StoredRuntimeSnapshot {
        active_threads: [
            ("child".into(), "worker".into()),
            ("unrelated".into(), "worker".into()),
        ]
        .into(),
        drained_envelopes: [("child".into(), vec![queued])].into(),
        agent_drained_envelopes: Default::default(),
    };
    storage
        .save_session_snapshot("root".into(), snapshot.clone())
        .await
        .unwrap();
    storage
        .abort_scope(ScopeAbort {
            task_id: task,
            members: vec![ScopeMember {
                thread_id: identity.thread_id.clone(),
                invocation_id: identity.invocation_id.clone(),
            }],
            reason: "checkpoint failed".into(),
        })
        .await
        .unwrap();
    let clean = storage.load_checkpoint("child").await.unwrap().unwrap();
    assert!(clean.active_execution.is_none());
    assert!(matches!(clean.resume_point, StoredResumePoint::Generation));
    assert!(
        matches!(&clean.messages.last().unwrap().message, Message::Tool(tool) if tool.id == "call" && matches!(tool.outcome, ToolCallOutcome::Aborted))
    );
    assert!(
        storage
            .save_execution_checkpoint(identity, child)
            .await
            .is_err()
    );
    storage
        .save_session_snapshot("root".into(), snapshot)
        .await
        .unwrap();
    let snapshot = storage
        .load_session_snapshot("root")
        .await
        .unwrap()
        .unwrap();
    assert!(!snapshot.active_threads.contains_key("child"));
    assert!(snapshot.active_threads.contains_key("unrelated"));
    assert!(snapshot.drained_envelopes.values().all(Vec::is_empty));
    assert!(
        storage
            .load_checkpoint("unrelated")
            .await
            .unwrap()
            .unwrap()
            .active_execution
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn notice_receipt_is_atomic_idempotent_and_survives_rewind() {
    let pool = pool().await;
    let workspace = workspace_id("notice_receipt");
    seed_session(&pool, &workspace, "root").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "root");
    let user_id = MessageId::new();
    let task = TaskId::new();
    let message_id = task.notice_message_id();
    let notice =
        coda_core::llm::TaskNoticeMessage::new(message_id, vec![], "full result".repeat(2000));
    let mut opening = checkpoint(
        "root",
        vec![
            entry(
                TurnId::from(user_id),
                Message::User(UserMessage::text(user_id, "start")),
            ),
            entry(TurnId::from(message_id), Message::TaskNotice(notice)),
        ],
    );
    opening.active_execution = Some(StoredExecution {
        invocation_id: "notice-invocation".into(),
        scope: ExecutionScope::Foreground {
            turn_id: TurnId::from(message_id),
        },
        completion: CompletionTarget::RootTurn,
        agent_path: vec!["coda".into()],
    });
    storage
        .admit_task_notice(task.clone(), opening.clone())
        .await
        .unwrap();
    storage
        .admit_task_notice(task.clone(), opening.clone())
        .await
        .unwrap();
    assert!(storage.has_notice_receipt(task.clone()).await.unwrap());
    assert_eq!(
        storage
            .load_checkpoint("root")
            .await
            .unwrap()
            .unwrap()
            .messages
            .len(),
        2
    );
    opening.active_execution = None;
    storage
        .save_checkpoint("root".into(), opening)
        .await
        .unwrap();
    let fork = WorkspaceStorage::new(pool.clone(), &workspace)
        .fork_session("root", ForkCut::All, ForkSource::Live)
        .await
        .unwrap();
    let fork_storage = PgSessionStorage::new(pool, &workspace, &fork.session_id);
    assert!(!fork_storage.has_notice_receipt(task.clone()).await.unwrap());
    storage.rewind_to(user_id).await.unwrap();
    assert!(storage.has_notice_receipt(task).await.unwrap());
    assert!(
        storage
            .load_checkpoint("root")
            .await
            .unwrap()
            .is_none_or(|checkpoint| checkpoint.messages.is_empty())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_notice_append_rolls_back_its_receipt() {
    let pool = pool().await;
    let workspace = workspace_id("notice_rollback");
    seed_session(&pool, &workspace, "root").await;
    let storage = PgSessionStorage::new(pool, &workspace, "root");
    let task = TaskId::new();
    let id = task.notice_message_id();
    let mut opening = checkpoint(
        "root",
        vec![entry(
            TurnId::from(id),
            Message::User(UserMessage::text(id, "existing")),
        )],
    );
    storage
        .save_checkpoint("root".into(), opening.clone())
        .await
        .unwrap();
    opening.messages.push(entry(
        TurnId::from(id),
        Message::TaskNotice(coda_core::llm::TaskNoticeMessage::new(
            id,
            vec![],
            "duplicate message id".into(),
        )),
    ));
    assert!(
        storage
            .admit_task_notice(task.clone(), opening)
            .await
            .is_err()
    );
    assert!(!storage.has_notice_receipt(task).await.unwrap());
    assert_eq!(
        storage
            .load_checkpoint("root")
            .await
            .unwrap()
            .unwrap()
            .messages
            .len(),
        1
    );
}
