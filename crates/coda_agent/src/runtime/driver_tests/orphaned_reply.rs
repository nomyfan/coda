//! Recovery when a checkpoint expects a reply whose producer died with the
//! previous process.

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

/// After a crash the sub-agent is simply gone —
/// no checkpoint, no envelope, nothing that will ever answer — so waiting for it
/// would wedge the session. That call is written off and the new work proceeds.
#[tokio::test]
async fn a_new_task_does_not_wait_for_a_sub_agent_that_no_longer_exists() {
    let storage = MemoryStorage::default();
    let (root, subagents) = coda_and_explore("hold-subagent");
    let team = crate::AgentTeam::new(root, subagents).expect("valid team");
    let harness = Harness::start_agents(
        storage.clone(),
        team.build(".", coda_tools::shared_file_locks(), test_registry()),
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
            team.build(".", coda_tools::shared_file_locks(), test_registry()),
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
