//! What happens when new work arrives at a thread that is still owed answers:
//! the turn in flight winds up first, and the new work waits its turn — unless
//! nothing in this process is left to answer, in which case waiting would be
//! waiting forever.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, SubAgentMode, ToolApprovalMode,
    persist::StoredResumePoint,
    runtime::{MemoryStorage, SessionStorage},
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{Duration, timeout};

fn coda_and_explore(explore_prompt: &str) -> (AgentSpec, Vec<AgentSpec>) {
    (
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
            system_prompt: explore_prompt.into(),
            mode: SubAgentMode::Stateless,
            tools: vec![],
            subagents: vec![],
        }],
    )
}

/// Sending a second message while a sub-agent is mid-write used to end the
/// first turn on the spot and start over. The sub-agent's work reached storage
/// afterwards, so the turn was announced finished before it was saved. Now the
/// new message waits for the old turn to wind up for real.
#[tokio::test]
async fn a_new_task_waits_for_the_turn_it_supersedes() {
    let storage = TestStorage::default();
    let (root, subagents) = coda_and_explore("explore-plain");
    let mut harness = Harness::start_with_team(
        storage.clone(),
        root,
        subagents,
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;
    let gate = storage.hold_checkpoints_of("explore").await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::ToolCallStart(call)) = (agent_name.as_str(), event)
                && call.name == "explore"
            {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the sub-agent call to go out");

    harness.send_task("actually, do this instead").await;

    // Nothing may finish while the sub-agent's work is still unwritten.
    let premature = timeout(Duration::from_millis(300), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::LLMEnd(msg)) = (agent_name.as_str(), event)
                && msg.tool_calls.is_empty()
            {
                return;
            }
        }
    })
    .await;
    assert!(
        premature.is_err(),
        "a turn ended while the sub-agent it superseded was still writing"
    );

    gate.release().await;

    let mut wound_up = false;
    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            match (agent_name.as_str(), event) {
                ("coda", AgentEvent::Aborted(_)) => wound_up = true,
                ("coda", AgentEvent::LLMEnd(msg)) if msg.tool_calls.is_empty() => return,
                (_, AgentEvent::PersistFailed(err)) => panic!("unexpected persist failure: {err}"),
                _ => {}
            }
        }
    })
    .await
    .expect("the superseding task never ran");
    assert!(
        wound_up,
        "the superseded turn never announced that it stopped"
    );

    let messages = storage
        .checkpoint(&harness.thread_id)
        .await
        .expect("root checkpoint")
        .messages;
    let submissions: Vec<&str> = messages
        .iter()
        .filter_map(|entry| match &entry.message {
            coda_core::llm::Message::User(user) => user.first_text(),
            _ => None,
        })
        .collect();
    assert_eq!(submissions, vec!["inspect", "actually, do this instead"]);

    harness.shutdown().await;
}

/// The other half of the same rule. After a crash the sub-agent is simply gone —
/// no checkpoint, no envelope, nothing that will ever answer — so waiting for it
/// would wedge the session. That call is written off and the new work proceeds.
#[tokio::test]
async fn a_new_task_does_not_wait_for_a_sub_agent_that_no_longer_exists() {
    let storage = MemoryStorage::default();
    let (root, subagents) = coda_and_explore("hold-subagent");
    let team = crate::AgentTeam::new(root, subagents).expect("valid team");
    let harness = Harness::start_agents(
        storage.clone(),
        team.build(".", coda_tools::shared_file_locks()),
        TestProvider::with_hold_subagent(Arc::new(tokio::sync::Notify::new())),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;

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
    assert!(
        storage
            .all_checkpoints()
            .await
            .iter()
            .all(|checkpoint| checkpoint.agent_name != "explore"),
        "the sub-agent should have written nothing yet, which is what makes it unrecoverable"
    );

    // A crash: the process goes away without draining anything, so the reopened
    // session has the root's outstanding call and no sub-agent behind it.
    let mut reopened = harness
        .restart(
            team.build(".", coda_tools::shared_file_locks()),
            TestProvider::with_hold_subagent(Arc::new(tokio::sync::Notify::new())),
            ToolApprovalMode::Auto,
            HashMap::new(),
        )
        .await;
    reopened.send_task("carry on").await;

    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = reopened.next_event().await;
            if let ("coda", AgentEvent::LLMEnd(msg)) = (agent_name.as_str(), event)
                && msg.tool_calls.is_empty()
            {
                return;
            }
        }
    })
    .await
    .expect("the reopened session hung waiting for a sub-agent that was never coming");
}
