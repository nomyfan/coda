//! The runtime's ledger of turns that have been submitted and not yet
//! finished: what opens one, what closes it, and which one an abort marks.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, ResumeDecision, SubAgentMode, ToolApprovalMode,
    ToolCallResolution,
    persist::StoredResumePoint,
    runtime::{AgentRuntimeSnapshot, MemoryStorage, SendCommandError, SessionStorage},
};
use std::sync::Arc;
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
            &pending.agent_name,
            &pending.thread_id,
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
        AgentTeam::new(root, subagents)
            .expect("valid team")
            .build(".", coda_tools::shared_file_locks()),
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
                .build(".", coda_tools::shared_file_locks()),
            TestProvider::with_hold_generation(Arc::new(tokio::sync::Notify::new())),
            approval,
            HashMap::from([(
                pending.thread_id.clone(),
                ResumeDecision {
                    resolutions: vec![(pending.calls[0].id.clone(), ToolCallResolution::Execute)],
                },
            )]),
        )
        .await;

    assert_eq!(active(&reopened.runtime), Some(interrupted));
    reopened.runtime.request_abort().await;
    assert!(cancelled(&reopened.runtime));
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
