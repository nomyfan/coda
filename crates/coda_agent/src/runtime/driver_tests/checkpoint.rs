//! Runtime lifecycle and checkpoint durability: exit timeout, a partial
//! stream error not leaking into history, error propagation to a parent
//! agent, and the user's task being durable before the turn completes.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, SubAgentMode, ToolApprovalMode,
    persist::StoredResumePoint,
    runtime::{MemoryStorage, SessionStorage},
};
use coda_core::llm::{Message, ToolOutput};
use std::collections::HashMap;
use std::sync::Arc;
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

/// Everything the runtime emits over a short window — for asserting about what
/// did *not* happen as much as what did.
async fn drain_events(harness: &mut Harness<TestStorage>) -> Vec<AgentEvent> {
    let mut seen = Vec::new();
    let _ = timeout(Duration::from_millis(200), async {
        loop {
            let (_, _, event) = harness.next_event().await;
            seen.push(event);
        }
    })
    .await;
    seen
}

fn persist_failures(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, AgentEvent::PersistFailed(_)))
        .count()
}

/// Drive events until the root agent ends its turn.
async fn root_turn_end(harness: &mut Harness<TestStorage>) {
    loop {
        let (agent_name, _, event) = harness.next_event().await;
        if let ("coda", AgentEvent::LLMEnd(message)) = (agent_name.as_str(), event)
            && message.tool_calls.is_empty()
        {
            return;
        }
    }
}

#[tokio::test]
async fn root_turn_cannot_end_while_a_subagent_checkpoint_is_unwritten() {
    // A sub-agent's reply is the only thing that lets its caller carry on, so
    // sending that reply only after the sub-agent's own checkpoint is durable
    // is what keeps the caller from ever getting ahead of the database.
    // Holding the write hostage is what makes the ordering observable: if the
    // reply still escapes, the root finishes the turn while the sub-agent's
    // history is nowhere on disk — which is exactly the copy `fork` would make.
    let storage = TestStorage::default();
    let gate = storage.hold_checkpoints_of("explore").await;

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

    assert!(
        timeout(Duration::from_millis(200), root_turn_end(&mut harness))
            .await
            .is_err(),
        "the root ended its turn while the sub-agent's checkpoint was still unwritten"
    );

    gate.release().await;

    timeout(Duration::from_secs(2), root_turn_end(&mut harness))
        .await
        .expect("the root never finished after the sub-agent's checkpoint landed");

    harness.shutdown().await;
}

/// A root agent that does one plain turn, so a test can aim a storage failure
/// at a chosen write and watch what the turn does about it.
async fn plain_root(storage: TestStorage, task: &str) -> Harness<TestStorage> {
    Harness::start_with_spec(
        storage,
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
        task,
    )
    .await
}

#[tokio::test]
async fn a_turn_that_cannot_store_its_prompt_never_starts() {
    // The opening write is the turn's first act. If the prompt is not on disk
    // there is nothing to hang the turn off, so calling the model would only
    // pile content onto a turn whose beginning does not exist — and burn tokens
    // doing it.
    let storage = TestStorage::default();
    storage.fail_checkpoints_after(0).await;
    let mut harness = plain_root(storage, "go").await;

    let events = drain_events(&mut harness).await;
    assert_eq!(persist_failures(&events), 1, "events: {events:?}");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::LLMStart(_))),
        "the model was called for a turn whose prompt was never stored: {events:?}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn a_turn_that_cannot_store_its_result_never_announces_it() {
    // The prompt lands, the model answers, and the closing write fails. The
    // reply exists only in memory at that point, so announcing it would tell
    // every reader the turn is done while the database says it never happened.
    let storage = TestStorage::default();
    storage.fail_checkpoints_after(1).await;
    let mut harness = plain_root(storage, "go").await;

    let events = drain_events(&mut harness).await;
    assert_eq!(persist_failures(&events), 1, "events: {events:?}");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::LLMStart(_))),
        "the turn should have got as far as calling the model: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::LLMEnd(_))),
        "the turn announced a result it had not stored: {events:?}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn an_unexpected_envelope_that_cannot_be_stored_reports_once() {
    // The third write point: `handle_envelope` bailing out early. It shares the
    // same helper as the other two, and this pins that it also reports the
    // failure exactly once rather than falling through the gap where there is
    // no turn ending to withhold.
    let storage = TestStorage::default();
    let mut harness = plain_root(storage.clone(), "go").await;
    timeout(Duration::from_secs(2), root_turn_end(&mut harness))
        .await
        .expect("the opening turn should finish");

    // Both of the opening turn's writes are spent; the next one fails.
    storage.fail_checkpoints_after(0).await;
    harness
        .runtime
        .send_message(Envelope::with_id(|id| Envelope {
            id,
            from: Sender::User,
            to: Receiver {
                name: "coda".into(),
                thread_id: harness.thread_id.clone(),
            },
            reply_to: None,
            body: EnvelopeBody::Reply {
                aborted: false,
                call_id: "nobody-asked".into(),
                output: ToolOutput::Ok("stray".into()),
            },
        }))
        .await
        .expect("send stray reply");

    let events = drain_events(&mut harness).await;
    assert_eq!(persist_failures(&events), 1, "events: {events:?}");

    harness.shutdown().await;
}

/// `docs/design/turn-cancellation.md` reads the ordered active turns off the
/// root's inbox, so the order tasks were submitted in has to survive being
/// snapshotted and replayed — otherwise "cancel the turn at the head" names the
/// wrong one after a restart.
#[tokio::test]
async fn tasks_queued_behind_the_exit_barrier_replay_in_order() {
    let storage = MemoryStorage::default();
    let coda = AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: "main-system".into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec!["explore".into()],
    };
    let explore = AgentSpec {
        name: "explore".into(),
        description: String::new(),
        system_prompt: "hold-subagent".into(),
        mode: SubAgentMode::Stateless,
        tools: vec![],
        subagents: vec![],
    };
    let team = AgentTeam::new(coda, vec![explore]).expect("valid team");
    let harness = Harness::start_agents(
        storage.clone(),
        team.build(".", coda_tools::shared_file_locks()),
        TestProvider::with_hold_subagent(Arc::new(tokio::sync::Notify::new())),
        ToolApprovalMode::Auto,
        "t1",
    )
    .await;

    // Park the root on a sub-agent reply that never comes, so the tasks below
    // pile up behind it instead of being consumed as they arrive.
    timeout(Duration::from_secs(2), async {
        loop {
            if let Some(checkpoint) = storage
                .load_checkpoint(harness.thread_id.as_ref())
                .await
                .expect("load checkpoint")
                && matches!(checkpoint.resume_point, StoredResumePoint::ToolExecution(ref state) if !state.pending_replies.is_empty())
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the root never parked on a pending reply");

    harness.runtime.request_exit().await;
    // Past the barrier these are buffered into the snapshot rather than delivered.
    harness.send_task("t2").await;
    harness.send_task("t3").await;
    harness
        .runtime
        .wait_for_exit(Some(Duration::from_secs(2)))
        .await;

    let mut harness = harness
        .restart(
            team.build(".", coda_tools::shared_file_locks()),
            TestProvider::with_hold_subagent(Arc::new(tokio::sync::Notify::new())),
            ToolApprovalMode::Auto,
            HashMap::new(),
        )
        .await;

    let mut answered = 0;
    timeout(Duration::from_secs(5), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::LLMEnd(msg)) = (agent_name.as_str(), event)
                && msg.tool_calls.is_empty()
            {
                answered += 1;
                if answered == 2 {
                    return;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for the replayed tasks to run");
    harness.shutdown().await;

    let root = storage
        .load_checkpoint(harness.thread_id.as_ref())
        .await
        .expect("load checkpoint")
        .expect("root thread was checkpointed");
    let submissions: Vec<&str> = root
        .messages
        .iter()
        .filter_map(|entry| match &entry.message {
            Message::User(user) => user.first_text(),
            _ => None,
        })
        .collect();
    assert_eq!(submissions, vec!["t1", "t2", "t3"]);
}
