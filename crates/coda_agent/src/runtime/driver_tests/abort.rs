//! Abort handling: cancelling a mix of local and sub-agent tool calls, a
//! cancel-aware tool settling with partial output, and aborting mid-generation.

use super::super::*;
use super::fixtures::*;
use crate::{
    AbortedTarget, AgentEvent, AgentSpec, StoredCheckpoint, SubAgentMode, ToolApprovalMode,
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
