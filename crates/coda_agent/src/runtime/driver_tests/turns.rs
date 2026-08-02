//! The runtime's ledger of turns that have been submitted and not yet
//! finished: what opens one, what closes it, and which one an abort marks.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, ResumeDecision, SubAgentMode, ToolApprovalMode,
    ToolCallResolution,
    persist::StoredResumePoint,
    runtime::{MemoryStorage, SendCommandError, SessionStorage},
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

fn active(runtime: &AgentRuntime) -> Vec<TurnId> {
    runtime.turns.lock().expect("active turns").order.clone()
}

fn cancelled(runtime: &AgentRuntime) -> HashSet<TurnId> {
    runtime
        .turns
        .lock()
        .expect("active turns")
        .cancelled
        .clone()
}

/// The turn to stop is the one at the head. Later submissions are separate
/// work the user has not taken back, so an abort must leave them alone.
#[tokio::test]
async fn an_abort_marks_the_turn_at_the_head_and_leaves_the_rest() {
    let runtime = AgentRuntime::new(MemoryStorage::default(), "session".into());
    let first = user_task(&ThreadId::from("session".to_string()));
    let second = user_task(&ThreadId::from("session".to_string()));
    runtime.open_turn(&first);
    runtime.open_turn(&second);

    // Nothing is bootstrapped, so the broadcast reaches nobody — which is the
    // point: the mark is the runtime's own record, not something an agent
    // installs on its way past.
    runtime.request_abort().await;

    assert_eq!(active(&runtime), vec![turn_of(&first), turn_of(&second)]);
    assert_eq!(cancelled(&runtime), HashSet::from([turn_of(&first)]));
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
    assert!(active(&runtime).is_empty());
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

    assert!(active(&harness.runtime).is_empty());
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
    assert_eq!(active(&harness.runtime), vec![turn]);
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
    assert_eq!(active(&harness.runtime).len(), 1);

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

    assert!(active(&harness.runtime).is_empty());
    harness.shutdown().await;
}

/// A submission that arrives instead of an approval decision used to discard
/// the parked calls and carry straight on, which ended the turn without ever
/// ending it: nothing announced that it stopped, so nothing closed it either.
/// It stayed at the head of the list, where every later abort would keep
/// marking it — leaving the turn actually running unstoppable.
#[tokio::test]
async fn a_task_that_supersedes_an_approval_closes_the_turn_it_replaced() {
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
    assert_eq!(active(&harness.runtime).len(), 1);

    harness.send_task("phase1").await;

    let mut stopped = false;
    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::Aborted(_)) => stopped = true,
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => return,
                _ => {}
            }
        }
    })
    .await
    .expect("the superseding task never finished");
    assert!(
        stopped,
        "the superseded approval never announced that its turn stopped"
    );
    assert!(
        active(&harness.runtime).is_empty(),
        "the superseded turn is still on the books: {:?}",
        active(&harness.runtime)
    );

    // The point of closing it: the next abort has to reach the turn that is
    // actually running rather than the one left at the head.
    harness.send_task("phase1").await;
    let running = active(&harness.runtime);
    harness.runtime.request_abort().await;
    assert_eq!(
        cancelled(&harness.runtime).into_iter().collect::<Vec<_>>(),
        running
    );

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

    assert_eq!(active(&reopened.runtime), vec![interrupted]);
    reopened.runtime.request_abort().await;
    assert_eq!(cancelled(&reopened.runtime), HashSet::from([interrupted]));
}

/// The stored snapshot is not cleared when it is replayed, so a second crash
/// hands the same envelopes back again. Registration keys on the turn id, so
/// replaying describes one turn rather than accumulating duplicates.
#[tokio::test]
async fn replaying_a_snapshot_twice_registers_one_turn() {
    let storage = MemoryStorage::default();
    // A root that never finishes a generation: the recovered submission below
    // starts its turn and leaves it open, so nothing closes underneath the
    // assertions.
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "abort-generation-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let stalls = || TestProvider::with_hold_generation(Arc::new(tokio::sync::Notify::new()));
    let mut harness = Harness::start_agents(
        storage.clone(),
        team.build(".", coda_tools::shared_file_locks()),
        stalls(),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;

    // Wait until the opening task is genuinely off the inbox and stuck in
    // generation. Exiting before that would drain it back into the snapshot,
    // and this test is about one recovered turn, not two.
    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::LLMContentChunk(_)) = (agent_name.as_str(), event) {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the opening turn to start generating");

    harness.runtime.request_exit().await;
    // Past the barrier this is buffered into the snapshot instead of delivered.
    let queued = user_task(&harness.thread_id);
    let queued_turn = turn_of(&queued);
    harness
        .runtime
        .send_message(queued)
        .await
        .expect("buffered past the exit barrier");
    harness
        .runtime
        .wait_for_exit(Some(Duration::from_millis(300)))
        .await;

    let first = harness
        .restart(
            team.build(".", coda_tools::shared_file_locks()),
            stalls(),
            ToolApprovalMode::Auto,
            HashMap::new(),
        )
        .await;
    assert_eq!(active(&first.runtime), vec![queued_turn]);

    // No graceful exit in between, so storage still holds the same snapshot.
    let second = first
        .restart(
            team.build(".", coda_tools::shared_file_locks()),
            stalls(),
            ToolApprovalMode::Auto,
            HashMap::new(),
        )
        .await;
    assert_eq!(active(&second.runtime), vec![queued_turn]);
}

/// Stopping the turn in flight is not a request to throw away what the user
/// queued behind it. The later submission is its own turn, so it keeps its
/// place and runs once the stopped one has wound up.
#[tokio::test]
async fn a_queued_task_survives_the_abort_of_the_one_ahead_of_it() {
    let mut harness = Harness::start_with_spec(
        MemoryStorage::default(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "abort-generation-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        TestProvider::with_hold_generation(Arc::new(tokio::sync::Notify::new())),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;

    // Wait until the first turn is genuinely generating, so the next task has
    // to queue behind it instead of being picked up straight away.
    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::LLMContentChunk(_)) = (agent_name.as_str(), event) {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the first turn to start generating");

    let queued = user_task(&harness.thread_id);
    let queued_turn = turn_of(&queued);
    harness
        .runtime
        .send_message(queued)
        .await
        .expect("queue a second task");
    assert_eq!(active(&harness.runtime).len(), 2);

    harness.runtime.request_abort().await;
    assert_eq!(cancelled(&harness.runtime).len(), 1);
    assert!(
        !cancelled(&harness.runtime).contains(&queued_turn),
        "the abort reached past the turn in flight"
    );

    // The stopped turn ends, and the queued one takes over.
    timeout(Duration::from_secs(2), async {
        let mut stopped = false;
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::Aborted(_)) => stopped = true,
                ("coda", AgentEvent::LLMStart(_)) if stopped => return,
                _ => {}
            }
        }
    })
    .await
    .expect("the queued task never started after the abort");

    assert_eq!(active(&harness.runtime), vec![queued_turn]);
    assert!(cancelled(&harness.runtime).is_empty());
    harness.shutdown().await;
}
