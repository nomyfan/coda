//! Suspend-for-approval mechanics at the driver level: mixed resolutions,
//! reject-via-restart, resuming across a restart vs. in-process, and
//! surviving suspension.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, SubAgentMode, ToolApprovalMode, ToolCallResolution,
    runtime::MemoryStorage,
};
use coda_core::llm::{ToolCall, ToolOutput};
use coda_tools::ReadTodosToolSpec;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::time::{Duration, timeout};

/// Cancellation cannot interrupt the write that records a suspension, so an
/// abort can land while a sub-agent parks for approval and the run still ends
/// parked. The snapshot has to keep that thread — reopening finds a sub-agent's
/// approval only through it.
#[tokio::test]
async fn an_abort_while_a_subagent_suspends_keeps_its_pending_approval() {
    let storage = TestStorage::default();
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let (root, subagents) = explore_read_todos_specs("main-system");
    let team = AgentTeam::new(root, subagents).expect("valid team");

    // A sub-agent has no user prompt to persist first, so its first checkpoint
    // write is the one recording the suspension. Holding it parks the run
    // exactly where an abort cannot turn it back.
    let gate = storage.hold_checkpoints_of("explore").await;
    let mut harness = Harness::start_agents(
        storage.clone(),
        team.build(".", coda_tools::shared_file_locks(), test_registry()),
        provider.clone(),
        approval.clone(),
        "inspect",
    )
    .await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("explore", AgentEvent::LLMEnd(_)) = (agent_name.as_str(), event) {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the sub-agent to ask for a tool");

    harness.runtime.cancel_in_flight().await;
    gate.release().await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("explore", AgentEvent::Suspended(_)) = (agent_name.as_str(), event) {
                return;
            }
        }
    })
    .await
    .expect("the sub-agent never parked for approval");
    harness.shutdown().await;

    // What proves the thread survived: the next process asks again.
    let mut harness = harness
        .restart(
            team.build(".", coda_tools::shared_file_locks(), test_registry()),
            provider,
            approval,
            HashMap::new(),
        )
        .await;
    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("explore", AgentEvent::Suspended(_)) = (agent_name.as_str(), event) {
                return;
            }
        }
    })
    .await
    .expect("the reopened session lost the sub-agent's pending approval");
    harness.shutdown().await;
}

/// The user's abort during that same write marks the turn stopped, and nothing
/// but a wind-up closes it. The parked thread has to be driven back in to do
/// that — no envelope is coming — or the turn stays open and the session
/// refuses every later task.
#[tokio::test]
async fn a_user_abort_while_a_subagent_suspends_still_ends_the_turn() {
    let storage = TestStorage::default();
    let (root, subagents) = explore_read_todos_specs("main-system");
    let team = AgentTeam::new(root, subagents).expect("valid team");

    let gate = storage.hold_checkpoints_of("explore").await;
    let mut harness = Harness::start_agents(
        storage.clone(),
        team.build(".", coda_tools::shared_file_locks(), test_registry()),
        TestProvider::default(),
        ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos")),
        "inspect",
    )
    .await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("explore", AgentEvent::LLMEnd(_)) = (agent_name.as_str(), event) {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the sub-agent to ask for a tool");

    harness.runtime.request_abort().await;
    gate.release().await;

    timeout(Duration::from_secs(2), async {
        while harness.runtime.turn_gate.active_id().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the stopped turn was never wound up, so the session takes no more tasks");

    harness.shutdown().await;
}

#[tokio::test]
async fn stateless_subagent_replies_after_approval_resume() {
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let (root, subagents) = explore_read_todos_specs("main-system");
    let team = AgentTeam::new(root, subagents).expect("valid team");
    let agents1 = team.build(".", coda_tools::shared_file_locks(), test_registry());
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        agents1,
        provider.clone(),
        approval.clone(),
        "inspect",
    )
    .await;

    // Phase 1: consume events until the explore subagent suspends for approval.
    let (pending, mut saw_subagent_tool) = {
        let result = timeout(Duration::from_secs(2), async {
            let mut saw_subagent_tool = false;
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                match (agent_name.as_str(), event) {
                    ("explore", AgentEvent::Suspended(pending)) => {
                        return (pending, saw_subagent_tool);
                    }
                    ("explore", AgentEvent::ToolCallEnd(tool)) if tool.name == "read_todos" => {
                        saw_subagent_tool = true;
                    }
                    _ => {}
                }
            }
        })
        .await;
        result.expect("timed out waiting for explore suspension")
    };
    harness.shutdown().await;

    // Phase 2: restart with resume, verify completion.
    let mut decisions = HashMap::new();
    decisions.insert(
        pending.agent_name.clone(),
        (
            pending.thread_id.clone(),
            ResumeDecision {
                parent_message_id: pending.parent_message_id,
                resolutions: vec![(pending.calls[0].id.clone(), ToolCallResolution::Execute)],
            },
        ),
    );
    let agents2 = team.build(".", coda_tools::shared_file_locks(), test_registry());
    let mut harness = harness
        .restart(agents2, provider, approval, decisions)
        .await;

    let mut saw_parent_tool_reply = false;
    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("explore", AgentEvent::ToolCallEnd(tool)) if tool.name == "read_todos" => {
                    saw_subagent_tool = true;
                }
                ("coda", AgentEvent::ToolCallEnd(tool)) if tool.name == "explore" => {
                    saw_parent_tool_reply = true;
                    assert!(matches!(tool.output, ToolOutput::Ok(ref s) if s == "explore done"));
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert!(
                        saw_subagent_tool,
                        "explore never finished its local tool call"
                    );
                    assert!(
                        saw_parent_tool_reply,
                        "coda never received the explore reply"
                    );
                    assert_eq!(msg.content, "main done");
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "timed out waiting for completion after resume"
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn pending_approval_supports_mixed_resolutions() {
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "approval-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec), Box::new(EchoToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let agents1 = team.build(".", coda_tools::shared_file_locks(), test_registry());
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        agents1,
        provider.clone(),
        approval.clone(),
        "inspect approvals",
    )
    .await;

    // Phase 1: consume until suspended, collect pending info.
    let (_pending_thread_id, decisions_map) = {
        let result = timeout(Duration::from_secs(2), async {
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                if let ("coda", AgentEvent::Suspended(pending)) = (agent_name.as_str(), event) {
                    assert_eq!(pending.calls.len(), 4);
                    let mut decisions = HashMap::new();
                    decisions.insert(
                        pending.agent_name.clone(),
                        (
                            pending.thread_id.clone(),
                            ResumeDecision {
                                parent_message_id: pending.parent_message_id,
                                resolutions: vec![
                                    ("call_exec".into(), ToolCallResolution::Execute),
                                    (
                                        "call_resolved".into(),
                                        ToolCallResolution::Resolved(ToolOutput::Ok(
                                            "resolved-by-test".into(),
                                        )),
                                    ),
                                    (
                                        "call_rejected".into(),
                                        ToolCallResolution::Rejected {
                                            reason: Some("nope".into()),
                                        },
                                    ),
                                ],
                            },
                        ),
                    );
                    return (pending.thread_id, decisions);
                }
            }
        })
        .await;
        result.expect("timed out waiting for suspension")
    };
    harness.shutdown().await;
    let agents2 = team.build(".", coda_tools::shared_file_locks(), test_registry());
    harness = harness
        .restart(agents2, provider, approval, decisions_map)
        .await;

    // Phase 2: consume events after resume, verify outcomes.
    let mut saw_tool_end_ids = HashSet::new();
    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::ToolCallEnd(tool)) => {
                    saw_tool_end_ids.insert(tool.id);
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert_eq!(msg.content, "approval-flow-ok");
                    assert!(saw_tool_end_ids.contains("call_exec"));
                    assert!(saw_tool_end_ids.contains("call_resolved"));
                    assert!(saw_tool_end_ids.contains("call_rejected"));
                    assert!(saw_tool_end_ids.contains("call_auto"));
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(result.is_ok(), "timed out waiting for mixed approval flow");
    harness.shutdown().await;
}

#[tokio::test]
async fn reject_pending_approval_via_restart() {
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "interrupt-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let agents1 = team.build(".", coda_tools::shared_file_locks(), test_registry());
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        agents1,
        provider.clone(),
        approval.clone(),
        "phase1",
    )
    .await;

    // Phase 1: consume until Suspended (read_todos needs approval).
    let pending = {
        let result = timeout(Duration::from_secs(2), async {
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                if let ("coda", AgentEvent::Suspended(p)) = (agent_name.as_str(), event) {
                    return p;
                }
            }
        })
        .await;
        result.expect("timed out waiting for suspension")
    };
    harness.shutdown().await;

    // Phase 2: reject the pending approval and restart.
    // The agent processes the rejection and continues with "phase1",
    // producing the final response.
    let mut reject_decisions = HashMap::new();
    let reject_ids: Vec<String> = pending.calls.iter().map(|c| c.id.clone()).collect();
    reject_decisions.insert(
        pending.agent_name.clone(),
        (
            pending.thread_id.clone(),
            ResumeDecision {
                parent_message_id: pending.parent_message_id,
                resolutions: reject_ids
                    .into_iter()
                    .map(|id| {
                        (
                            id,
                            ToolCallResolution::Rejected {
                                reason: Some("replaced by new task".into()),
                            },
                        )
                    })
                    .collect(),
            },
        ),
    );
    let agents2 = team.build(".", coda_tools::shared_file_locks(), test_registry());
    let mut harness = harness
        .restart(agents2, provider, approval, reject_decisions)
        .await;

    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert_eq!(msg.content, "interrupt-flow-ok");
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "timed out waiting for completion after reject"
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn restart_re_emits_pending_approval_with_original_suspended_at() {
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "interrupt-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let agents1 = team.build(".", coda_tools::shared_file_locks(), test_registry());
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        agents1,
        provider.clone(),
        approval.clone(),
        "phase1",
    )
    .await;

    let first_pending = {
        let result = timeout(Duration::from_secs(2), async {
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                if let ("coda", AgentEvent::Suspended(p)) = (agent_name.as_str(), event) {
                    return p;
                }
            }
        })
        .await;
        result.expect("timed out waiting for first suspension")
    };
    harness.shutdown().await;

    let agents2 = team.build(".", coda_tools::shared_file_locks(), test_registry());
    let mut harness = harness
        .restart(agents2, provider, approval, HashMap::new())
        .await;

    let resumed_pending = {
        let result = timeout(Duration::from_secs(2), async {
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                if let ("coda", AgentEvent::Suspended(p)) = (agent_name.as_str(), event) {
                    return p;
                }
            }
        })
        .await;
        result.expect("timed out waiting for resumed suspension")
    };

    assert_eq!(resumed_pending.suspended_at, first_pending.suspended_at);
    harness.shutdown().await;
}

/// The runtime snapshot is only written when an agent exits, so a process
/// killed mid-approval — and a session a fork has just minted — comes back with
/// checkpoints but no snapshot at all. A decision names the thread it belongs
/// to, so it must still reach it: routing it through the snapshot's
/// `active_threads` dropped it silently and left the thread parked forever,
/// with every later approval swallowed the same way.
#[tokio::test]
async fn an_approval_resumes_a_session_that_never_wrote_a_runtime_snapshot() {
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "continuation-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        team.build(".", coda_tools::shared_file_locks(), test_registry()),
        provider.clone(),
        approval.clone(),
        "inspect todos",
    )
    .await;

    let pending = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::Suspended(pending)) = (agent_name.as_str(), event) {
                break pending;
            }
        }
    })
    .await
    .expect("timed out waiting for tool approval");
    harness.shutdown().await;

    let decisions = [(
        pending.agent_name.clone(),
        (
            pending.thread_id.clone(),
            ResumeDecision {
                parent_message_id: pending.parent_message_id,
                resolutions: vec![(pending.calls[0].id.clone(), ToolCallResolution::Execute)],
            },
        ),
    )]
    .into();
    let mut harness = harness
        .restart_without_snapshot(
            team.build(".", coda_tools::shared_file_locks(), test_registry()),
            provider,
            approval,
            decisions,
        )
        .await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::ToolCallEnd(tool)) = (agent_name.as_str(), event) {
                assert_eq!(tool.name, "read_todos");
                assert!(
                    matches!(tool.outcome, ToolCallOutcome::Approved),
                    "the approved call must run, not be rejected for want of a decision"
                );
                break;
            }
        }
    })
    .await
    .expect("the approval was swallowed: nothing ran after the resume");
    harness.shutdown().await;
}

#[tokio::test]
async fn restart_replays_reasoning_continuation_after_tool_approval() {
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "continuation-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        team.build(".", coda_tools::shared_file_locks(), test_registry()),
        provider.clone(),
        approval.clone(),
        "inspect todos",
    )
    .await;

    let pending = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::Suspended(pending)) = (agent_name.as_str(), event) {
                break pending;
            }
        }
    })
    .await
    .expect("timed out waiting for tool approval");
    harness.shutdown().await;

    let decisions = [(
        pending.agent_name.clone(),
        (
            pending.thread_id.clone(),
            ResumeDecision {
                parent_message_id: pending.parent_message_id,
                resolutions: vec![(pending.calls[0].id.clone(), ToolCallResolution::Execute)],
            },
        ),
    )]
    .into();
    let mut harness = harness
        .restart(
            team.build(".", coda_tools::shared_file_locks(), test_registry()),
            provider,
            approval,
            decisions,
        )
        .await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::LLMEnd(message)) = (agent_name.as_str(), event)
                && message.tool_calls.is_empty()
            {
                assert_eq!(message.content, "continuation-restored-ok");
                break;
            }
        }
    })
    .await
    .expect("timed out waiting for completion after restored continuation");
    harness.shutdown().await;
}

#[tokio::test]
async fn in_process_resume_after_suspension() {
    // Verify that after an agent suspends for approval, sending a Resume
    // envelope in-process (without shutdown/restart) allows the turn to
    // complete normally.
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "interrupt-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let provider = TestProvider::default();
    let approval =
        ToolApprovalMode::RequireWhen(Arc::new(|call: &ToolCall| call.name == "read_todos"));
    let agents = team.build(".", coda_tools::shared_file_locks(), test_registry());
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        agents,
        provider,
        approval,
        "phase1",
    )
    .await;

    // Wait for the Suspended event.
    let pending = {
        let result = timeout(Duration::from_secs(2), async {
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                if let ("coda", AgentEvent::Suspended(p)) = (agent_name.as_str(), event) {
                    return p;
                }
            }
        })
        .await;
        result.expect("timed out waiting for suspension")
    };

    // Resume in-process — no shutdown/restart.
    harness
        .send_resume(
            &pending,
            vec![(pending.calls[0].id.clone(), ToolCallResolution::Execute)],
        )
        .await;

    // Verify the turn completes after in-process resume.
    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::LLMEnd(msg)) = (agent_name.as_str(), event)
                && msg.tool_calls.is_empty()
            {
                assert_eq!(msg.content, "interrupt-flow-ok");
                return;
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "timed out waiting for completion after in-process resume"
    );

    harness.shutdown().await;
}

/// "A call the decision does not name is rejected" holds all the way to naming
/// none of them: an empty `resolutions` for the batch that is actually parked
/// rejects it wholesale. Only the batch id decides whether a decision applies,
/// so this stays a legitimate way to refuse everything.
#[tokio::test]
async fn an_empty_decision_for_the_parked_batch_rejects_it() {
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "two-batch-approval".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        team.build(".", coda_tools::shared_file_locks(), test_registry()),
        TestProvider::default(),
        ToolApprovalMode::RequireWhen(Arc::new(|call: &ToolCall| call.name == "read_todos")),
        "two batches",
    )
    .await;

    let pending = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::Suspended(pending)) = (agent_name.as_str(), event) {
                break pending;
            }
        }
    })
    .await
    .expect("timed out waiting for the suspension");

    harness.send_resume(&pending, vec![]).await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::ToolCallEnd(tool)) = (agent_name.as_str(), event) {
                assert!(
                    matches!(tool.outcome, ToolCallOutcome::Rejected { .. }),
                    "an unnamed call must still be rejected, got {:?}",
                    tool.outcome
                );
                break;
            }
        }
    })
    .await
    .expect("the empty decision was ignored instead of rejecting the batch");
    harness.shutdown().await;
}

#[tokio::test]
async fn a_resume_meant_for_an_earlier_batch_does_not_reject_the_current_one() {
    // Submitting the same approval twice (a double-clicked button, a retry
    // after a reconnect) sends the first batch's decision a second time. By
    // then the thread has run those calls and suspended on the model's next
    // batch — and every call in that batch is unnamed by the stale decision,
    // so applying it rejected the lot and told the model the user had refused.
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "two-batch-approval".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team");
    let approval =
        ToolApprovalMode::RequireWhen(Arc::new(|call: &ToolCall| call.name == "read_todos"));
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        team.build(".", coda_tools::shared_file_locks(), test_registry()),
        TestProvider::default(),
        approval,
        "two batches",
    )
    .await;

    let next_suspension = async |harness: &mut Harness<MemoryStorage>| loop {
        let (agent_name, _, event) = harness.next_event().await;
        if let ("coda", AgentEvent::Suspended(pending)) = (agent_name.as_str(), event) {
            return pending;
        }
    };

    let first = timeout(Duration::from_secs(2), next_suspension(&mut harness))
        .await
        .expect("timed out waiting for the first suspension");
    let first_decision = vec![(first.calls[0].id.clone(), ToolCallResolution::Execute)];
    harness.send_resume(&first, first_decision.clone()).await;

    let second = timeout(Duration::from_secs(2), next_suspension(&mut harness))
        .await
        .expect("timed out waiting for the second suspension");
    // The two batches reuse one call id, so the ids cannot tell a stale
    // decision from a live one — which is the whole reason the batch is
    // identified by the message that asked for it.
    assert_eq!(second.calls[0].id, first.calls[0].id);
    assert_ne!(second.parent_message_id, first.parent_message_id);

    // The duplicate submit: the same approval answered a second time, landing
    // on the batch the thread moved on to.
    let stale = harness
        .runtime
        .send_message(Envelope::with_id(|id| Envelope {
            id,
            from: Sender::User,
            to: Receiver {
                name: first.agent_name.clone(),
                thread_id: ThreadId::from(first.thread_id.clone()),
            },
            reply_to: None,
            body: EnvelopeBody::Resume(crate::ResumeDecision {
                parent_message_id: first.parent_message_id,
                resolutions: first_decision,
            }),
        }))
        .await;
    assert!(matches!(
        stale,
        Err(crate::runtime::SendCommandError::StaleApproval)
    ));
    let reannounced = harness
        .runtime
        .pending_approvals()
        .into_iter()
        .find(|a| a.thread_id == second.thread_id)
        .unwrap();
    assert_eq!(reannounced.parent_message_id, second.parent_message_id);

    // Answering the batch that is actually parked still works.
    harness
        .send_resume(
            &reannounced,
            vec![(reannounced.calls[0].id.clone(), ToolCallResolution::Execute)],
        )
        .await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::ToolCallEnd(tool)) => assert!(
                    matches!(tool.outcome, ToolCallOutcome::Approved),
                    "{} was answered with {:?}",
                    tool.id,
                    tool.outcome
                ),
                ("coda", AgentEvent::LLMEnd(message)) if message.tool_calls.is_empty() => {
                    assert_eq!(message.content, "two-batch-done");
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for the turn to finish");
    harness.shutdown().await;
}
