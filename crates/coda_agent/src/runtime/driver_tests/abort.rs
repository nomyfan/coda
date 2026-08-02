//! Abort handling: cancelling a mix of local and sub-agent tool calls, a
//! cancel-aware tool settling with partial output, and aborting mid-generation.

use super::super::*;
use super::fixtures::*;
use crate::{
    AbortedTarget, AgentEvent, AgentSpec, StoredCheckpoint, SubAgentMode, ToolApprovalMode,
    runtime::MemoryStorage,
};
use coda_core::llm::{Message, ToolCallOutcome, ToolOutput};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::yield_now;
use tokio::time::{Duration, timeout};

/// A checkpoint's conversation without the turn tags, for assertions that only
/// care about the messages themselves.
fn messages_of(checkpoint: &StoredCheckpoint) -> Vec<Message> {
    checkpoint
        .messages
        .iter()
        .map(|entry| entry.message.clone())
        .collect()
}

#[tokio::test]
async fn abort_during_mixed_tool_execution_aborts_local_and_subagent_calls() {
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_team(
        storage.clone(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "abort-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(SlowToolSpec {
                gate: Arc::new(Notify::new()),
            })],
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
        TestProvider::with_hold_subagent(Arc::new(Notify::new())),
        ToolApprovalMode::Auto,
        "abort",
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        let mut started = std::collections::HashSet::new();
        let mut ended = std::collections::HashSet::new();

        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::ToolCallStart(tool)) => {
                    started.insert(tool.id);
                    if started.contains("call_slow") && started.contains("call_explore") {
                        harness.runtime.request_abort().await;
                    }
                }
                ("coda", AgentEvent::ToolCallEnd(tool))
                    if matches!(tool.outcome, ToolCallOutcome::Aborted) =>
                {
                    ended.insert(tool.id);
                }
                ("coda", AgentEvent::Aborted(AbortedTarget::ToolCalls(ids))) => {
                    assert!(ids.contains(&"call_slow".to_string()));
                    assert!(ids.contains(&"call_explore".to_string()));
                    // Every aborted ToolMessage written to history must have
                    // been announced via ToolCallEnd before the Aborted marker.
                    assert!(ended.contains("call_slow"));
                    assert!(ended.contains("call_explore"));
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    let checkpoint = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(checkpoint) = harness.storage.checkpoint(&harness.thread_id).await
                && matches!(
                    checkpoint.resume_point,
                    crate::persist::StoredResumePoint::Generation
                )
            {
                break checkpoint;
            }
            yield_now().await;
        }
    })
    .await
    .expect("checkpoint was not saved after abort");

    harness.shutdown().await;
    result.expect("timed out waiting for abort event");
    assert!(matches!(
        tool_message(&messages_of(&checkpoint), "call_slow"),
        Some(tool) if matches!(tool.outcome, ToolCallOutcome::Aborted)
    ));
    assert!(matches!(
        tool_message(&messages_of(&checkpoint), "call_explore"),
        Some(tool) if matches!(tool.outcome, ToolCallOutcome::Aborted)
    ));
}

#[tokio::test]
async fn abort_settles_cancel_aware_tool_with_partial_output() {
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_spec(
        storage.clone(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "abort-cancel-aware-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![Box::new(CancelAwareToolSpec)],
            subagents: vec![],
        },
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "abort cancel aware",
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        let mut saw_end = false;
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::ToolCallStart(tool)) if tool.id == "call_cancel" => {
                    harness.runtime.request_abort().await;
                }
                ("coda", AgentEvent::ToolCallEnd(tool)) if tool.id == "call_cancel" => {
                    // The tool observed the cancellation and settled itself:
                    // its salvaged partial output is recorded, not the generic
                    // interruption message.
                    assert!(matches!(tool.outcome, ToolCallOutcome::Aborted));
                    assert!(matches!(
                        &tool.output,
                        ToolOutput::Err(reason) if reason.contains("partial output from teardown")
                    ));
                    saw_end = true;
                }
                ("coda", AgentEvent::Aborted(AbortedTarget::ToolCalls(ids))) => {
                    assert!(ids.contains(&"call_cancel".to_string()));
                    assert!(saw_end, "ToolCallEnd must precede the Aborted marker");
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    let checkpoint = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(checkpoint) = harness.storage.checkpoint(&harness.thread_id).await
                && matches!(
                    checkpoint.resume_point,
                    crate::persist::StoredResumePoint::Generation
                )
            {
                break checkpoint;
            }
            yield_now().await;
        }
    })
    .await
    .expect("checkpoint was not saved after abort");

    harness.shutdown().await;
    result.expect("timed out waiting for abort event");
    assert!(matches!(
        tool_message(&messages_of(&checkpoint), "call_cancel"),
        Some(tool) if matches!(tool.outcome, ToolCallOutcome::Aborted)
            && matches!(
                &tool.output,
                ToolOutput::Err(reason) if reason.contains("partial output from teardown")
            )
    ));
}

#[tokio::test]
async fn abort_during_generation_emits_aborted_and_persists_partial_message() {
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_spec(
        storage.clone(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "abort-generation-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        TestProvider::with_hold_generation(Arc::new(Notify::new())),
        ToolApprovalMode::Auto,
        "abort generation",
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        let mut saw_chunk = false;
        let mut saw_reasoning = false;
        let mut saw_aborted_llm_end = false;

        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::LLMReasoningChunk(chunk)) => {
                    assert_eq!(chunk, "partial reasoning");
                    saw_reasoning = true;
                }
                ("coda", AgentEvent::LLMContentChunk(chunk)) => {
                    assert_eq!(chunk, "partial");
                    saw_chunk = true;
                    harness.runtime.request_abort().await;
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.aborted => {
                    assert!(msg.content.contains("partial"));
                    saw_aborted_llm_end = true;
                }
                ("coda", AgentEvent::Aborted(AbortedTarget::Generation)) => {
                    assert!(
                        saw_chunk,
                        "generation was aborted before any partial content"
                    );
                    assert!(
                        saw_reasoning,
                        "generation was aborted before any partial reasoning"
                    );
                    // The aborted partial message written to history must have
                    // been announced via LLMEnd before the Aborted marker.
                    assert!(
                        saw_aborted_llm_end,
                        "no LLMEnd was emitted for the aborted partial message"
                    );
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    let checkpoint = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(checkpoint) = harness.storage.checkpoint(&harness.thread_id).await
                && let Some(Message::Assistant(message)) =
                    checkpoint.messages.last().map(|entry| &entry.message)
                && message.aborted
            {
                break checkpoint;
            }
            yield_now().await;
        }
    })
    .await
    .expect("checkpoint was not saved after generation abort");

    harness.shutdown().await;
    result.expect("timed out waiting for generation abort");
    assert!(matches!(
        checkpoint.resume_point,
        crate::persist::StoredResumePoint::Generation
    ));
    assert!(matches!(
        checkpoint.messages.last().map(|entry| &entry.message),
        Some(Message::Assistant(message))
            if message.aborted
                && message.content.contains("partial")
                && message.content.contains("interrupted by the user")
                && message.reasoning_content.as_deref() == Some("partial reasoning")
    ));
}

/// An abort travels up the call tree, not across it: each thread waits for the
/// sub-agents it already dispatched to answer for themselves. The deepest one
/// here cannot write, so nothing above it may declare the turn over — its work
/// is not in storage yet, and the whole point of announcing last is that what
/// the user sees is already saved.
#[tokio::test]
async fn a_root_abort_waits_for_the_bottom_of_the_tree() {
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_team(
        storage.clone(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "main-system".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec!["explore".into()],
        },
        vec![
            AgentSpec {
                name: "explore".into(),
                description: String::new(),
                system_prompt: "nested-explore".into(),
                mode: SubAgentMode::Stateful,
                tools: vec![],
                subagents: vec!["probe".into()],
            },
            AgentSpec {
                name: "probe".into(),
                description: String::new(),
                system_prompt: "explore-plain".into(),
                mode: SubAgentMode::Stateless,
                tools: vec![],
                subagents: vec![],
            },
        ],
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;
    let gate = storage.hold_checkpoints_of("probe").await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("explore", AgentEvent::ToolCallStart(call)) = (agent_name.as_str(), event)
                && call.name == "probe"
            {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the deepest call to be dispatched");

    harness.runtime.request_abort().await;

    // Nothing may end the turn while the bottom of the tree is still writing.
    let premature = timeout(Duration::from_millis(300), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::Aborted(_)) = (agent_name.as_str(), event) {
                return;
            }
        }
    })
    .await;
    assert!(
        premature.is_err(),
        "the root announced the abort before the deepest thread was durable"
    );

    gate.release().await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::Aborted(_)) => return,
                (_, AgentEvent::PersistFailed(err)) => panic!("unexpected persist failure: {err}"),
                _ => {}
            }
        }
    })
    .await
    .expect("the root never announced the abort once the write landed");

    assert!(
        storage.checkpoint(&harness.thread_id).await.is_some(),
        "the root's own state should be durable too"
    );
    harness.shutdown().await;
}

/// Aborting instead of answering an approval prompt is an ordinary thing to do.
/// The parked sub-agent has no envelope coming to wake it, so the abort has to
/// push it — and it must wind up and reply rather than sit there until the root
/// gives up and reports a persistence failure.
#[tokio::test]
async fn aborting_instead_of_answering_an_approval_winds_the_subagent_up() {
    let (root, subagents) = explore_read_todos_specs("main-system");
    let mut harness = Harness::start_with_team(
        MemoryStorage::default(),
        root,
        subagents,
        TestProvider::default(),
        ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos")),
        "inspect",
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
    .expect("timed out waiting for the sub-agent to suspend");

    // No resume decision — the user stops the turn instead of answering.
    harness.runtime.request_abort().await;

    let mut subagent_wound_up = false;
    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("explore", AgentEvent::Aborted(_)) => subagent_wound_up = true,
                ("coda", AgentEvent::Aborted(_)) => return,
                (_, AgentEvent::PersistFailed(err)) => panic!("unexpected persist failure: {err}"),
                _ => {}
            }
        }
    })
    .await
    .expect("the parked sub-agent was never pushed to wind up");
    assert!(
        subagent_wound_up,
        "the root ended the turn without the sub-agent answering for itself"
    );

    harness.shutdown().await;
}

/// A sub-agent that never answers must not keep the root waiting forever. What
/// the root says when it gives up matters as much as that it does: the turn's
/// content never reached storage, so it reports that and lets the session
/// rebuild from what really is there. Announcing the turn stopped would be a
/// success signal for work that was never saved.
#[tokio::test]
async fn a_sub_agent_that_never_answers_does_not_pin_the_root() {
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_team(
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
            system_prompt: "explore-plain".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![],
            subagents: vec![],
        }],
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;
    // Never released: the sub-agent is stuck saving, so it can neither finish
    // nor answer, which is the only way a wind-up genuinely hangs.
    let _wedged = storage.hold_checkpoints_of("explore").await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("explore", AgentEvent::LLMStart(_)) = (agent_name.as_str(), event) {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the sub-agent to start");

    harness.runtime.request_abort().await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::PersistFailed(_)) => return,
                ("coda", AgentEvent::Aborted(_)) => {
                    panic!("the root announced the turn stopped without its content being saved")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the root waited on the wedged sub-agent indefinitely");
}
