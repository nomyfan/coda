//! Runtime lifecycle and checkpoint durability: exit timeout, a partial
//! stream error not leaking into history, error propagation to a parent
//! agent, and the user's task being durable before the turn completes.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, SubAgentMode, ToolApprovalMode, runtime::MemoryStorage,
};
use coda_core::llm::{Message, ToolOutput};
use std::collections::HashMap;
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn wait_for_exit_honors_timeout_and_completes_after_exit() {
    let agents = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "main-system".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        vec![],
    )
    .expect("valid team")
    .build(".", coda_tools::shared_file_locks());

    let config = test_config(TestProvider::default(), ToolApprovalMode::Auto);

    let mut runtime = AgentRuntime::new(MemoryStorage::default(), "test-session".into());
    runtime
        .bootstrap(agents, None, HashMap::new(), config)
        .await;

    assert!(!runtime.wait_for_exit(Some(Duration::from_millis(20))).await);

    runtime.request_exit().await;
    assert!(runtime.wait_for_exit(Some(Duration::from_secs(2))).await);
}

#[tokio::test]
async fn partial_stream_error_does_not_enter_history_or_checkpoint() {
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_spec(
        storage.clone(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "partial-error-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "trigger partial error",
    )
    .await;

    let mut saw_reasoning = false;
    let mut saw_content = false;
    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::LLMReasoningChunk(chunk)) => {
                    assert_eq!(chunk, "uncommitted reasoning");
                    saw_reasoning = true;
                }
                ("coda", AgentEvent::LLMContentChunk(chunk)) => {
                    assert_eq!(chunk, "uncommitted content");
                    saw_content = true;
                }
                ("coda", AgentEvent::Error(error)) => {
                    assert!(saw_reasoning && saw_content);
                    assert_eq!(
                        error,
                        "Provider test-provider error 502 (upstream_error): upstream disconnected"
                    );
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for partial stream error");

    let checkpoint = storage
        .checkpoint(&harness.thread_id)
        .await
        .expect("user task should remain checkpointed");
    assert!(matches!(
        checkpoint.messages.last().map(|entry| &entry.message),
        Some(Message::User(user)) if user.first_text() == Some("trigger partial error")
    ));
    assert!(!checkpoint.messages.iter().any(|entry| matches!(
        &entry.message,
        Message::Assistant(assistant)
            if assistant.content.contains("uncommitted")
                || assistant.reasoning_content.as_deref().is_some_and(|value| value.contains("uncommitted"))
    )));

    harness.shutdown().await;
}

#[tokio::test]
async fn llm_errors_surface_for_root_agent_and_reply_to_parent_agent() {
    let mut root = Harness::start_with_spec(
        MemoryStorage::default(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "error-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        },
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "root error",
    )
    .await;

    let root_result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = root.next_event().await;
            if agent_name == "coda"
                && let AgentEvent::Error(err) = event
            {
                assert_eq!(err, "Invalid response: main boom");
                break;
            }
        }
    })
    .await;
    root.shutdown().await;
    root_result.expect("timed out waiting for root agent error");

    let mut parent = Harness::start_with_team(
        MemoryStorage::default(),
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "error-parent-main".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec!["explore".into()],
        },
        vec![AgentSpec {
            name: "explore".into(),
            description: String::new(),
            system_prompt: "error-subagent".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![],
            subagents: vec![],
        }],
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "subagent error",
    )
    .await;

    let parent_result = timeout(Duration::from_secs(2), async {
        let mut saw_explore_error = false;

        loop {
            let (agent_name, _, event) = parent.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::ToolCallEnd(tool)) if tool.name == "explore" => {
                    saw_explore_error = true;
                    assert!(matches!(
                        tool.output,
                        ToolOutput::Err(ref out) if out == "Transport error: subagent boom"
                    ));
                }
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => {
                    assert!(
                        saw_explore_error,
                        "parent never received the subagent error"
                    );
                    assert_eq!(msg.content, "subagent-error-ok");
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    parent.shutdown().await;
    parent_result.expect("timed out waiting for subagent error reply");
}

#[tokio::test]
async fn user_task_is_checkpointed_before_turn_completes() {
    // The user prompt must be durable as soon as the turn starts — a mid-turn
    // crash or reconnect must not lose it. `abort-generation-main` holds the
    // LLM stream open, so observing the first chunk proves the turn is still
    // in flight when we inspect the checkpoint.
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
        TestProvider::with_hold_generation(std::sync::Arc::new(tokio::sync::Notify::new())),
        ToolApprovalMode::Auto,
        "hold this task",
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if agent_name == "coda" && matches!(event, AgentEvent::LLMContentChunk(_)) {
                break;
            }
        }
    })
    .await;
    result.expect("timed out waiting for generation to start");

    let checkpoint = harness
        .storage
        .checkpoint(&harness.thread_id)
        .await
        .expect("user task was not checkpointed at turn start");
    assert!(matches!(
        checkpoint.messages.last().map(|entry| &entry.message),
        Some(Message::User(user)) if user.first_text() == Some("hold this task")
    ));
    assert!(matches!(
        checkpoint.resume_point,
        crate::persist::StoredResumePoint::Generation
    ));

    harness.shutdown().await;
}
