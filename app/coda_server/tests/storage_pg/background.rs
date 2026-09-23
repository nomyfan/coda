use super::*;
use coda_agent::execution::{
    CompletionTarget, ExecutionIdentity, ProcessGroupId, ScopeAbort, StoredExecution,
};
use coda_core::task::{ScopeMember, TaskId};

fn execution(task: &TaskId, invocation: &str) -> StoredExecution {
    StoredExecution {
        invocation_id: invocation.into(),
        scope: ProcessGroupId::Background {
            task_id: task.clone(),
        },
        completion: CompletionTarget::BackgroundTask(task.clone()),
        agent_path: vec!["coda".into(), "worker".into()],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn old_group_abort_preserves_a_reused_process_checkpoint_and_inbox() {
    let pool = pool().await;
    let workspace = workspace_id("reused_process");
    seed_session(&pool, &workspace, "root").await;
    let storage = PgSessionStorage::new(pool, &workspace, "root");
    let old = TaskId::new();
    let new = TaskId::new();
    let mut child = checkpoint("child", vec![]);
    child.agent_name = "worker".into();
    child.parent_pid = Some("root".into());
    child.derivation_key = Some("worker".into());
    child.active_execution = Some(execution(&new, "new-invocation"));
    storage
        .save_checkpoint("child".into(), child.clone())
        .await
        .unwrap();
    let mut queued = queued_task("child", "new work");
    queued.id = "new-invocation".into();
    let mut resume = queued_task("child", "new approval");
    resume.body = EnvelopeBody::Resume(coda_agent::ResumeDecision {
        parent_message_id: MessageId::new(),
        resolutions: vec![],
    });
    let resume_id = resume.id.clone();
    storage
        .save_session_snapshot(
            "root".into(),
            StoredRuntimeSnapshot {
                active_processes: [("child".into(), "worker".into())].into(),
                drained_envelopes: [("child".into(), vec![queued, resume.clone()])].into(),
                agent_drained_envelopes: [("child".into(), vec![resume])].into(),
            },
        )
        .await
        .unwrap();
    storage
        .abort_scope(ScopeAbort {
            task_id: old,
            members: vec![ScopeMember {
                pid: "child".into(),
                invocation_id: "old-invocation".into(),
            }],
            reason: "old group failed".into(),
        })
        .await
        .unwrap();
    let current = storage.load_checkpoint("child").await.unwrap().unwrap();
    assert_eq!(
        current.active_execution.unwrap().invocation_id,
        "new-invocation"
    );
    let snapshot = storage
        .load_session_snapshot("root")
        .await
        .unwrap()
        .unwrap();
    assert!(snapshot.active_processes.contains_key("child"));
    assert_eq!(snapshot.drained_envelopes["child"][0].id, "new-invocation");
    assert_eq!(snapshot.drained_envelopes["child"][1].id, resume_id);
    assert_eq!(snapshot.agent_drained_envelopes["child"][0].id, resume_id);
    assert!(
        storage
            .save_execution_checkpoint(
                ExecutionIdentity {
                    pid: "child".into(),
                    invocation_id: "old-invocation".into(),
                },
                child
            )
            .await
            .is_err()
    );
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
        output_bytes: None,
        id: "call".into(),
        name: "shell".into(),
        arguments: Some("{}".into()),
    });
    let mut child = checkpoint(
        "child",
        vec![entry(turn, Message::Assistant(assistant.clone()))],
    );
    child.agent_name = "worker".into();
    child.parent_pid = Some("root".into());
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
        pid: "child".into(),
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
    let mut resume = queued_task("child", "old approval");
    resume.body = EnvelopeBody::Resume(coda_agent::ResumeDecision {
        parent_message_id: assistant.message_id,
        resolutions: vec![],
    });
    assert_ne!(resume.id, identity.invocation_id);
    assert!(resume.reply_to.is_none());
    let snapshot = StoredRuntimeSnapshot {
        active_processes: [
            ("child".into(), "worker".into()),
            ("unrelated".into(), "worker".into()),
        ]
        .into(),
        drained_envelopes: [("child".into(), vec![queued, resume.clone()])].into(),
        agent_drained_envelopes: [("child".into(), vec![resume])].into(),
    };
    storage
        .save_session_snapshot("root".into(), snapshot.clone())
        .await
        .unwrap();
    storage
        .abort_scope(ScopeAbort {
            task_id: task,
            members: vec![ScopeMember {
                pid: identity.pid.clone(),
                invocation_id: identity.invocation_id.clone(),
            }],
            reason: "checkpoint failed".into(),
        })
        .await
        .unwrap();
    let cleaned_snapshot = storage
        .load_session_snapshot("root")
        .await
        .unwrap()
        .unwrap();
    assert!(
        cleaned_snapshot
            .drained_envelopes
            .values()
            .all(Vec::is_empty)
    );
    assert!(
        cleaned_snapshot
            .agent_drained_envelopes
            .values()
            .all(Vec::is_empty)
    );
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
    assert!(!snapshot.active_processes.contains_key("child"));
    assert!(snapshot.active_processes.contains_key("unrelated"));
    assert!(snapshot.drained_envelopes.values().all(Vec::is_empty));
    assert!(snapshot.agent_drained_envelopes.values().all(Vec::is_empty));
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
        scope: ProcessGroupId::Foreground {
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

fn observed_read(task: &TaskId) -> Message {
    let mut message = ToolMessage::new(
        "read",
        "task_output",
        ToolOutput::Ok("complete terminal output".into()),
        ToolCallOutcome::Auto,
        None,
    );
    message.observed_task = Some(task.clone());
    Message::Tool(message)
}

#[tokio::test(flavor = "multi_thread")]
async fn root_task_read_receipt_survives_reopen_and_rewind_but_not_fork() {
    let pool = pool().await;
    let workspace = workspace_id("task_read");
    seed_session(&pool, &workspace, "root").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "root");
    let task = TaskId::new();
    let user_id = MessageId::new();
    let turn = TurnId::from(user_id);
    let root = checkpoint(
        "root",
        vec![
            entry(
                turn,
                Message::User(UserMessage::text(user_id, "read result")),
            ),
            entry(turn, observed_read(&task)),
        ],
    );
    storage.save_checkpoint("root".into(), root).await.unwrap();
    let reopened = PgSessionStorage::new(pool.clone(), &workspace, "root");
    assert!(reopened.has_notice_receipt(task.clone()).await.unwrap());
    let fork = WorkspaceStorage::new(pool.clone(), &workspace)
        .fork_session("root", ForkCut::All, ForkSource::Live)
        .await
        .unwrap();
    let fork_storage = PgSessionStorage::new(pool, &workspace, &fork.session_id);
    assert!(!fork_storage.has_notice_receipt(task.clone()).await.unwrap());
    let mut fork_checkpoint = fork_storage
        .load_checkpoint(&fork.session_id)
        .await
        .unwrap()
        .unwrap();
    fork_checkpoint
        .messages
        .push(entry(turn, assistant("next response")));
    fork_storage
        .save_checkpoint(fork.session_id, fork_checkpoint)
        .await
        .unwrap();
    assert!(
        !fork_storage.has_notice_receipt(task.clone()).await.unwrap(),
        "saving a fork must not re-acknowledge copied history"
    );
    reopened.rewind_to(user_id).await.unwrap();
    assert!(reopened.has_notice_receipt(task).await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_checkpoint_rolls_back_the_task_read_receipt() {
    let pool = pool().await;
    let workspace = workspace_id("task_read_rollback");
    seed_session(&pool, &workspace, "root").await;
    let storage = PgSessionStorage::new(pool, &workspace, "root");
    let task = TaskId::new();
    let user_id = MessageId::new();
    let turn = TurnId::from(user_id);
    let user = entry(turn, Message::User(UserMessage::text(user_id, "start")));
    storage
        .save_checkpoint("root".into(), checkpoint("root", vec![user.clone()]))
        .await
        .unwrap();
    let failed = checkpoint(
        "root",
        vec![user.clone(), entry(turn, observed_read(&task)), user],
    );
    assert!(
        storage
            .save_checkpoint("root".into(), failed)
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

#[tokio::test(flavor = "multi_thread")]
async fn owning_process_task_read_receipt_is_persisted() {
    let pool = pool().await;
    let workspace = workspace_id("owner_task_read");
    seed_session(&pool, &workspace, "root").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "root");
    let task = TaskId::new();
    let mut child = checkpoint(
        "child",
        vec![entry(TurnId::from(MessageId::new()), observed_read(&task))],
    );
    child.parent_pid = Some("root".into());
    child.derivation_key = Some("child".into());
    storage
        .save_checkpoint("child".into(), child)
        .await
        .unwrap();
    let reopened = PgSessionStorage::new(pool, &workspace, "root");
    assert!(reopened.has_notice_receipt(task).await.unwrap());
}

fn page_read(task: &TaskId, consumer: &str, start: u64, end: u64, total: u64) -> ToolMessage {
    use coda_core::output::{Channel, ReadReceipt};
    let mut tool = ToolMessage::new(
        "page",
        "task_output",
        ToolOutput::Ok("bounded page".into()),
        ToolCallOutcome::Auto,
        None,
    );
    tool.read_receipts.push(ReadReceipt {
        consumer: consumer.into(),
        task: task.clone(),
        channel: Channel::Stdout,
        start,
        end,
        total,
        terminal: true,
        complete: end == total,
    });
    if end == total {
        tool.observed_tasks.push(task.clone());
    }
    tool
}

#[tokio::test(flavor = "multi_thread")]
async fn output_progress_and_paths_survive_rewind_while_forks_share_only_the_files() {
    use coda_core::output::{FINALIZE_TIMEOUT, OutputData, OutputLimits, OutputOwner, OutputStore};
    let pool = pool().await;
    let workspace = workspace_id("output_progress_paths");
    seed_session(&pool, &workspace, "root").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "root");
    let task = TaskId::new();
    let user = MessageId::new();
    let turn = TurnId::from(user);
    let dir = tempfile::tempdir().unwrap();
    let store = coda_output::Store::open(OutputLimits {
        root: dir.path().join("output"),
        ..OutputLimits::default()
    })
    .unwrap();
    let OutputData::Captured(output) = store
        .retain(
            OutputOwner {
                workspace_id: workspace.clone(),
                session_id: "root".into(),
            },
            "shared middle log".repeat(2000),
            16 * 1024,
            tokio::time::Instant::now() + FINALIZE_TIMEOUT,
        )
        .await
    else {
        panic!()
    };
    let reference = output.reference.unwrap();
    let mut tool = page_read(&task, "root", 0, 20, 20);
    tool.output_refs.push(reference.clone());
    storage
        .save_checkpoint(
            "root".into(),
            checkpoint(
                "root",
                vec![
                    entry(turn, Message::User(UserMessage::text(user, "read"))),
                    entry(turn, Message::Tool(tool)),
                ],
            ),
        )
        .await
        .unwrap();
    let reopened = PgSessionStorage::new(pool.clone(), &workspace, "root");
    assert_eq!(
        reopened.load_output_progress("root").await.unwrap()[0].offset,
        20
    );
    assert!(reopened.has_notice_receipt(task.clone()).await.unwrap());
    let workspace_storage = WorkspaceStorage::new(pool.clone(), &workspace);
    let charged = store.charged_bytes();
    let fork = workspace_storage
        .fork_session("root", ForkCut::All, ForkSource::Live)
        .await
        .unwrap();
    let fork_storage = PgSessionStorage::new(pool.clone(), &workspace, &fork.session_id);
    assert!(
        fork_storage
            .load_output_progress(&fork.session_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!fork_storage.has_notice_receipt(task).await.unwrap());
    let saved = fork_storage
        .load_checkpoint(&fork.session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&saved.messages[1].message, Message::Tool(t) if t.output_refs == vec![reference.clone()])
    );
    reopened.rewind_to(user).await.unwrap();
    assert_eq!(
        reopened.load_output_progress("root").await.unwrap()[0].offset,
        20
    );
    workspace_storage.delete_session("root").await.unwrap();
    assert!(
        reopened
            .load_output_progress("root")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.charged_bytes(), charged);
    assert_eq!(
        std::fs::read_to_string(&reference.channels[0].path).unwrap(),
        "shared middle log".repeat(2000)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_output_checkpoint_rolls_back_progress_and_notification_together() {
    let pool = pool().await;
    let workspace = workspace_id("output_progress_rollback");
    seed_session(&pool, &workspace, "root").await;
    let storage = PgSessionStorage::new(pool, &workspace, "root");
    let task = TaskId::new();
    let id = MessageId::new();
    let turn = TurnId::from(id);
    let first = entry(turn, Message::User(UserMessage::text(id, "read")));
    storage
        .save_checkpoint("root".into(), checkpoint("root", vec![first.clone()]))
        .await
        .unwrap();
    let tool = entry(turn, Message::Tool(page_read(&task, "root", 0, 100, 100)));
    assert!(
        storage
            .save_checkpoint(
                "root".into(),
                checkpoint("root", vec![first.clone(), tool.clone(), first.clone()])
            )
            .await
            .is_err()
    );
    assert!(
        storage
            .load_output_progress("root")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!storage.has_notice_receipt(task.clone()).await.unwrap());
    let final_checkpoint = checkpoint("root", vec![first, tool]);
    storage
        .save_checkpoint("root".into(), final_checkpoint.clone())
        .await
        .unwrap();
    storage
        .save_checkpoint("root".into(), final_checkpoint)
        .await
        .unwrap();
    assert_eq!(
        storage.load_output_progress("root").await.unwrap()[0].offset,
        100
    );
    assert!(storage.has_notice_receipt(task).await.unwrap());
}
