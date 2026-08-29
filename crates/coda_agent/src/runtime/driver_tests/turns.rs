//! The runtime's ledger of turns that have been submitted and not yet
//! finished: what opens one, what closes it, and which one an abort marks.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, ResumeDecision, SubAgentMode, ToolApprovalMode,
    ToolCallResolution,
    agent::HistoryEntry,
    persist::{StoredCheckpoint, StoredPreparedToolCall, StoredResumePoint},
    runtime::{
        AgentRuntimeSnapshot, MemoryStorage, ResumeTarget, SendCommandError, SessionStorage,
    },
};
use coda_core::llm::{Message, ToolCall, UserMessage};
use std::{collections::HashMap, sync::Arc};
use tokio::time::{Duration, timeout};

fn user_task(to: &ThreadId) -> Envelope {
    Envelope::with_id(|id| Envelope {
        id,
        from: Sender::User,
        to: Receiver {
            name: "coda".into(),
            thread_id: to.clone(),
        },
        reply_to: None,
        body: EnvelopeBody::Task {
            message_id: MessageId::new(),
            task: "inspect".into(),
            images: vec![],
            author: Default::default(),
        },
    })
}

fn turn_of(envelope: &Envelope) -> TurnId {
    let EnvelopeBody::Task { message_id, .. } = &envelope.body else {
        panic!("not a task envelope");
    };
    TurnId::from(*message_id)
}

fn active(runtime: &AgentRuntime) -> Option<TurnId> {
    runtime.turn_gate.active_id()
}

fn cancelled(runtime: &AgentRuntime) -> bool {
    active(runtime).is_some_and(|turn| runtime.turn_gate.is_cancelled(turn))
}

#[tokio::test]
async fn a_second_task_is_rejected_and_abort_marks_the_active_turn() {
    let runtime = AgentRuntime::new(MemoryStorage::default(), "session".into());
    let first = user_task(&ThreadId::from("session".to_string()));
    let second = user_task(&ThreadId::from("session".to_string()));
    runtime.turn_gate.open(turn_of(&first)).expect("open first");
    assert!(matches!(
        runtime.send_message(second).await,
        Err(SendCommandError::TurnAlreadyActive)
    ));

    runtime.request_abort().await;

    assert_eq!(active(&runtime), Some(turn_of(&first)));
    assert!(cancelled(&runtime));
}

/// Registering before delivering means a delivery that fails would otherwise
/// strand a turn nobody will ever finish — and a stranded head is permanent,
/// since every later abort would keep marking it.
#[tokio::test]
async fn a_delivery_that_fails_leaves_no_turn_behind() {
    let runtime = AgentRuntime::new(MemoryStorage::default(), "session".into());

    let sent = runtime
        .send_message(user_task(&ThreadId::from("session".to_string())))
        .await;

    assert!(matches!(sent, Err(SendCommandError::AgentNotFound)));
    assert_eq!(active(&runtime), None);
}

#[tokio::test]
async fn an_answered_turn_leaves_the_active_list() {
    let mut harness = Harness::start_with_spec(
        MemoryStorage::default(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "plain-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::LLMEnd(msg)) = (agent_name.as_str(), event)
                && msg.tool_calls.is_empty()
            {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the root to answer");

    assert_eq!(active(&harness.runtime), None);
    harness.shutdown().await;
}

/// The root goes idle while its sub-agents work, but the turn is not over —
/// nothing has been announced for it. Closing it here would leave an abort
/// with nothing to mark for the whole time the sub-agents are running.
#[tokio::test]
async fn a_turn_waiting_on_a_subagent_stays_active() {
    let storage = MemoryStorage::default();
    let harness = Harness::start_with_team(
        storage.clone(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "main-system".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec!["explore".into()],
        },
        vec![AgentSpec {
            name: "explore".into(),
            description: String::new(),
            system_prompt: "hold-subagent".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![],
            subagents: vec![],
        }],
        TestProvider::with_hold_subagent(Arc::new(tokio::sync::Notify::new())),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;

    let parked = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(checkpoint) = storage
                .load_checkpoint(harness.thread_id.as_ref())
                .await
                .expect("load checkpoint")
                && matches!(checkpoint.resume_point, StoredResumePoint::ToolExecution(ref state) if !state.pending_replies.is_empty())
            {
                return checkpoint;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the root never parked on a pending reply");

    let turn = parked
        .messages
        .last()
        .expect("the parked thread has history")
        .turn_id;
    assert_eq!(active(&harness.runtime), Some(turn));
}

/// A `Resume` continues the turn that suspended; it must not look like a fresh
/// submission, or the ledger would gain a phantom entry that never closes.
#[tokio::test]
async fn resuming_an_approval_does_not_open_a_second_turn() {
    let mut harness = Harness::start_with_spec(
        MemoryStorage::default(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "approval-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![
                Box::new(coda_tools::ReadTodosToolSpec),
                Box::new(EchoToolSpec),
            ],
            subagents: vec![],
        },
        TestProvider::default(),
        ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos")),
        "inspect",
    )
    .await;

    let pending = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::Suspended(pending)) = (agent_name.as_str(), event) {
                return pending;
            }
        }
    })
    .await
    .expect("timed out waiting for approval suspension");

    // Suspension parks the turn; it does not end it.
    assert!(active(&harness.runtime).is_some());

    harness
        .send_resume(
            &pending,
            pending
                .calls
                .iter()
                .map(|call| (call.id.clone(), ToolCallResolution::Execute))
                .collect(),
        )
        .await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::LLMEnd(msg)) = (agent_name.as_str(), event)
                && msg.tool_calls.is_empty()
            {
                return msg.content;
            }
        }
    })
    .await
    .expect("timed out waiting for completion after resume");

    assert_eq!(active(&harness.runtime), None);
    harness.shutdown().await;
}

#[tokio::test]
async fn a_task_is_rejected_while_an_approval_is_pending() {
    let mut harness = Harness::start_with_spec(
        MemoryStorage::default(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "interrupt-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(coda_tools::ReadTodosToolSpec)],
            subagents: vec![],
        },
        TestProvider::default(),
        ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos")),
        "phase1",
    )
    .await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::Suspended(_)) = (agent_name.as_str(), event) {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for approval suspension");
    let running = active(&harness.runtime).expect("approval keeps the turn active");
    let sent = harness
        .runtime
        .send_message(user_task(&harness.thread_id))
        .await;
    assert!(matches!(sent, Err(SendCommandError::TurnAlreadyActive)));
    assert_eq!(active(&harness.runtime), Some(running));

    harness.runtime.request_abort().await;
    assert!(cancelled(&harness.runtime));

    harness.shutdown().await;
}

/// Reopening a session has to put the interrupted turn back on the books before
/// any agent can move: the user's first act after reopening may well be to stop
/// it, and an abort that finds an empty list marks nothing.
#[tokio::test]
async fn a_restart_puts_the_interrupted_turn_back() {
    let storage = MemoryStorage::default();
    let (root, subagents) = explore_read_todos_specs("main-system");
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let mut harness = Harness::start_agents(
        storage.clone(),
        AgentTeam::new(root, subagents).expect("valid team").build(
            ".",
            coda_tools::shared_file_locks(),
            test_registry(),
        ),
        TestProvider::default(),
        approval.clone(),
        "inspect",
    )
    .await;

    // The sub-agent parks for approval, which is the state a reopened session
    // resumes from.
    let pending = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("explore", AgentEvent::Suspended(pending)) = (agent_name.as_str(), event) {
                return pending;
            }
        }
    })
    .await
    .expect("timed out waiting for approval suspension");
    let interrupted = storage
        .load_checkpoint(harness.thread_id.as_ref())
        .await
        .expect("load checkpoint")
        .expect("root thread was checkpointed")
        .messages
        .last()
        .expect("the root has history")
        .turn_id;
    harness.shutdown().await;

    // The reopened root cannot get past its next generation, so the resumed
    // turn stays open for the whole assertion rather than racing it closed.
    let (mut stalled_root, resumed_subagents) = explore_read_todos_specs("abort-generation-main");
    stalled_root.subagents = vec!["explore".into()];
    let reopened = harness
        .restart(
            AgentTeam::new(stalled_root, resumed_subagents)
                .expect("valid team")
                .build(".", coda_tools::shared_file_locks(), test_registry()),
            TestProvider::with_hold_generation(Arc::new(tokio::sync::Notify::new())),
            approval,
            HashMap::from([(
                pending.agent_name.clone(),
                (
                    pending.thread_id.clone(),
                    ResumeDecision {
                        parent_message_id: pending.parent_message_id,
                        resolutions: vec![(
                            pending.calls[0].id.clone(),
                            ToolCallResolution::Execute,
                        )],
                    },
                ),
            )]),
        )
        .await;

    assert_eq!(active(&reopened.runtime), Some(interrupted));
    reopened.runtime.request_abort().await;
    assert!(cancelled(&reopened.runtime));
}

/// Same requirement, for the session that has no snapshot to put anything back
/// from: a fork, or a process killed while the root sat on an approval. The
/// resume decision is then the only evidence the turn is still running, so the
/// checkpoint it names is what has to fill the slot — otherwise the resumed work
/// runs outside single-flight and the next task opens a second turn alongside it.
#[tokio::test]
async fn a_resume_without_a_snapshot_puts_the_interrupted_turn_back() {
    let storage = MemoryStorage::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let root_running = |system_prompt: &str| AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: system_prompt.into(),
        mode: SubAgentMode::Stateful,
        tools: vec![Box::new(coda_tools::ReadTodosToolSpec)],
        subagents: vec![],
    };
    let mut harness = Harness::start_agents(
        storage.clone(),
        AgentTeam::new(root_running("continuation-main"), vec![])
            .expect("valid team")
            .build(".", coda_tools::shared_file_locks(), test_registry()),
        TestProvider::default(),
        approval.clone(),
        "inspect todos",
    )
    .await;

    let pending = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::Suspended(pending)) = (agent_name.as_str(), event) {
                return pending;
            }
        }
    })
    .await
    .expect("timed out waiting for approval suspension");
    let interrupted = storage
        .load_checkpoint(harness.thread_id.as_ref())
        .await
        .expect("load checkpoint")
        .expect("root thread was checkpointed")
        .messages
        .last()
        .expect("the root has history")
        .turn_id;
    harness.shutdown().await;

    // The resumed root stalls in the generation that follows the approved call,
    // so the turn stays open for the whole assertion rather than racing it shut.
    let reopened = harness
        .restart_without_snapshot(
            AgentTeam::new(root_running("abort-generation-main"), vec![])
                .expect("valid team")
                .build(".", coda_tools::shared_file_locks(), test_registry()),
            TestProvider::with_hold_generation(Arc::new(tokio::sync::Notify::new())),
            approval,
            HashMap::from([(
                pending.agent_name.clone(),
                (
                    pending.thread_id.clone(),
                    ResumeDecision {
                        parent_message_id: pending.parent_message_id,
                        resolutions: vec![(
                            pending.calls[0].id.clone(),
                            ToolCallResolution::Execute,
                        )],
                    },
                ),
            )]),
        )
        .await;

    assert_eq!(active(&reopened.runtime), Some(interrupted));
    assert!(matches!(
        reopened
            .runtime
            .send_message(user_task(&reopened.thread_id))
            .await,
        Err(SendCommandError::TurnAlreadyActive)
    ));
}

#[tokio::test]
async fn a_resume_target_overrides_the_same_agents_stale_snapshot_thread() {
    let storage = MemoryStorage::default();
    let old_thread = "old-stateless-thread";
    let current_thread = "current-stateless-thread";
    let old_prompt = MessageId::new();
    let current_prompt = MessageId::new();
    let old_turn = TurnId::from(old_prompt);
    let current_turn = TurnId::from(current_prompt);
    let parent_message_id = MessageId::new();
    let call = ToolCall {
        id: "call_read_todos".into(),
        name: "read_todos".into(),
        arguments: Some("{}".into()),
    };

    storage
        .save_checkpoint(
            old_thread.into(),
            StoredCheckpoint {
                thread_id: old_thread.into(),
                agent_name: "explore".into(),
                parent_thread_id: Some("session".into()),
                derivation_key: Some("old-call".into()),
                reply_target: None,
                messages: vec![HistoryEntry::new(
                    old_turn,
                    Message::User(UserMessage::text(old_prompt, "old turn")),
                )],
                resume_point: StoredResumePoint::Generation,
                suspended_at: jiff::Timestamp::default(),
            },
        )
        .await
        .expect("save stale checkpoint");
    storage
        .save_checkpoint(
            current_thread.into(),
            StoredCheckpoint {
                thread_id: current_thread.into(),
                agent_name: "explore".into(),
                parent_thread_id: Some("session".into()),
                derivation_key: Some("current-call".into()),
                reply_target: None,
                messages: vec![
                    HistoryEntry::new(
                        current_turn,
                        Message::User(UserMessage::text(current_prompt, "current turn")),
                    ),
                    HistoryEntry::new(
                        current_turn,
                        Message::Assistant(AssistantMessage {
                            message_id: parent_message_id,
                            tool_calls: vec![call.clone()],
                            ..assistant()
                        }),
                    ),
                ],
                resume_point: StoredResumePoint::PendingApproval {
                    parent_message_id,
                    pending_approval_calls: vec![StoredPreparedToolCall {
                        tool_call: call.clone(),
                        metadata: None,
                    }],
                    pending_calls: vec![],
                },
                suspended_at: jiff::Timestamp::now(),
            },
        )
        .await
        .expect("save current checkpoint");

    let snapshot = AgentRuntimeSnapshot {
        active_threads: HashMap::from([("explore".into(), old_thread.into())]),
        ..Default::default()
    };
    let resume_targets = HashMap::from([(
        "explore".into(),
        ResumeTarget {
            thread_id: ThreadId::from(current_thread.to_string()),
            decision: ResumeDecision {
                parent_message_id,
                resolutions: vec![(call.id, ToolCallResolution::Execute)],
            },
        },
    )]);
    let agents = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "plain-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec!["explore".into()],
        },
        vec![AgentSpec {
            name: "explore".into(),
            description: String::new(),
            system_prompt: "abort-generation-main".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![Box::new(coda_tools::ReadTodosToolSpec)],
            subagents: vec![],
        }],
    )
    .expect("valid team")
    .build(".", coda_tools::shared_file_locks(), test_registry());
    let mut runtime = AgentRuntime::new(storage, "session".into());
    runtime
        .bootstrap(
            agents,
            Some(snapshot),
            resume_targets,
            test_config(
                TestProvider::with_hold_generation(Arc::new(tokio::sync::Notify::new())),
                ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos")),
            ),
        )
        .await
        .expect("the authoritative resume target should replace the stale snapshot thread");

    assert_eq!(active(&runtime), Some(current_turn));
    runtime.cancel_in_flight().await;
    runtime.request_exit().await;
    assert!(runtime.wait_for_exit(Some(Duration::from_secs(2))).await);
}

#[tokio::test]
async fn repeated_recovery_evidence_registers_one_turn() {
    let runtime = AgentRuntime::new(MemoryStorage::default(), "session".into());
    let task = user_task(&ThreadId::from("session".to_string()));
    let turn = turn_of(&task);
    let snapshot = AgentRuntimeSnapshot {
        drained_envelopes: HashMap::from([("coda".into(), vec![task.clone(), task])]),
        ..Default::default()
    };

    let checkpoints = runtime
        .recovery_checkpoints(&snapshot)
        .await
        .expect("load checkpoints");
    runtime
        .register_resumed_work(&snapshot, &checkpoints)
        .expect("same turn is idempotent");

    assert_eq!(active(&runtime), Some(turn));
}

#[tokio::test]
async fn recovery_rejects_multiple_turns() {
    let runtime = AgentRuntime::new(MemoryStorage::default(), "session".into());
    let first = user_task(&ThreadId::from("session".to_string()));
    let second = user_task(&ThreadId::from("session".to_string()));
    let snapshot = AgentRuntimeSnapshot {
        drained_envelopes: HashMap::from([("coda".into(), vec![first, second])]),
        ..Default::default()
    };

    let checkpoints = runtime
        .recovery_checkpoints(&snapshot)
        .await
        .expect("load checkpoints");
    let error = runtime
        .register_resumed_work(&snapshot, &checkpoints)
        .expect_err("multiple turns must not be replayed");

    assert!(error.contains("active turns"), "{error}");
}
