//! Which thread a sub-agent invocation runs in, and what it records about
//! who called it: stateful vs. stateless derivation, reused call ids, nested
//! delegation, and turn tagging across threads.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, SubAgentMode, ToolApprovalMode, ToolCallResolution,
    persist::StoredResumePoint,
    runtime::{MemoryStorage, SessionStorage},
};
use coda_core::llm::{Message, MessageId, MessageOrigin, ToolOutput, TurnId};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{Duration, timeout};

/// A root plus one `explore` sub-agent that answers without calling any tools.
fn explore_specs(main_prompt: &str, mode: SubAgentMode) -> (AgentSpec, Vec<AgentSpec>) {
    let coda = AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: main_prompt.into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec!["explore".into()],
    };
    let explore = AgentSpec {
        name: "explore".into(),
        description: String::new(),
        system_prompt: "explore-plain".into(),
        mode,
        tools: vec![],
        subagents: vec![],
    };
    (coda, vec![explore])
}

/// The `(parent message id, call id)` pair recorded on each user message that
/// opens work in a thread, in order.
async fn origins_in_thread(
    storage: &MemoryStorage,
    thread_id: &ThreadId,
) -> Vec<Option<MessageOrigin>> {
    storage
        .load_checkpoint(thread_id.as_ref())
        .await
        .expect("load checkpoint")
        .expect("thread was checkpointed")
        .messages
        .iter()
        .filter_map(|entry| match &entry.message {
            Message::User(user) => Some(user.origin.clone()),
            _ => None,
        })
        .collect()
}

/// Every assistant message in a thread that issued tool calls, as
/// `(message id, the ids of the calls it issued)`.
async fn tool_calling_assistants(
    storage: &MemoryStorage,
    thread_id: &ThreadId,
) -> Vec<(MessageId, Vec<String>)> {
    storage
        .load_checkpoint(thread_id.as_ref())
        .await
        .expect("load checkpoint")
        .expect("thread was checkpointed")
        .messages
        .iter()
        .filter_map(|entry| match &entry.message {
            Message::Assistant(a) if !a.tool_calls.is_empty() => Some((
                a.message_id,
                a.tool_calls.iter().map(|c| c.id.clone()).collect(),
            )),
            _ => None,
        })
        .collect()
}

async fn wait_for_completion_after_explore_reply(
    harness: &mut Harness<MemoryStorage>,
    require_resume: bool,
) {
    let mut approval_resumed = false;
    let mut saw_subagent_tool = false;
    let mut saw_parent_tool_reply = false;
    let mut observed = Vec::new();

    let result = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            observed.push(format!("{} {:?}", agent_name, event));
            match (agent_name.as_str(), event) {
                ("explore", AgentEvent::Suspended(pending)) if require_resume => {
                    approval_resumed = true;
                    harness
                        .send_resume(
                            &pending,
                            vec![(pending.calls[0].id.clone(), ToolCallResolution::Execute)],
                        )
                        .await;
                }
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
                    if require_resume {
                        assert!(approval_resumed, "explore was never resumed after approval");
                    }
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    if let Err(err) = result {
        panic!(
            "timed out waiting for explore completion: {err:?}; observed events: {}",
            observed.join(" | ")
        );
    }
}

#[tokio::test]
async fn stateless_subagent_replies_after_local_tool_execution() {
    let (root, subagents) = explore_read_todos_specs("main-system");
    let mut harness = Harness::start_with_team(
        MemoryStorage::default(),
        root,
        subagents,
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;
    wait_for_completion_after_explore_reply(&mut harness, false).await;
    harness.shutdown().await;
}

/// A stateful sub-agent keeps one thread across calls, so two invocations pile
/// their messages into the same history. What tells them apart is the origin
/// recorded on each opening message — and it has to be the *pair*
/// `(parent message id, call id)`, because this script reuses one call id for
/// both invocations, which is legal.
#[tokio::test]
async fn stateful_subagent_records_which_call_opened_each_invocation() {
    let (root, subagents) = explore_specs("twice-main", SubAgentMode::Stateful);
    let mut harness = Harness::start_with_team(
        MemoryStorage::default(),
        root,
        subagents,
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;

    let done = timeout(Duration::from_secs(2), async {
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
    .expect("timed out waiting for the root agent to finish");
    assert_eq!(done, "main done");
    harness.shutdown().await;

    let parents = tool_calling_assistants(&harness.storage, &harness.thread_id).await;
    assert_eq!(
        parents.len(),
        2,
        "the root should have called explore twice"
    );
    // The premise of the test: the two calls are indistinguishable by call id.
    assert_eq!(parents[0].1, parents[1].1);

    // Both invocations share one thread, and each opening message points back at
    // the assistant message that issued it.
    let explore_thread = ThreadId::from_uuid5(&harness.thread_id, "explore");
    assert_eq!(
        origins_in_thread(&harness.storage, &explore_thread).await,
        vec![
            Some(MessageOrigin {
                message_id: parents[0].0,
                call_id: "call_explore".into(),
            }),
            Some(MessageOrigin {
                message_id: parents[1].0,
                call_id: "call_explore".into(),
            }),
        ]
    );
}

/// One submission's work fans out across threads — the root's, a stateful
/// sub-agent's, a nested stateless one's — and a rewind has to find all of it
/// starting from the submission alone. So every message any of those threads
/// writes while serving one task carries that task's turn.
#[tokio::test]
async fn one_submission_tags_every_thread_it_reaches() {
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
        system_prompt: "nested-explore".into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec!["probe".into()],
    };
    let probe = AgentSpec {
        name: "probe".into(),
        description: String::new(),
        system_prompt: "explore-plain".into(),
        mode: SubAgentMode::Stateless,
        tools: vec![],
        subagents: vec![],
    };
    let mut harness = Harness::start_with_team(
        MemoryStorage::default(),
        coda,
        vec![explore, probe],
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
    .expect("timed out waiting for the root agent to finish");
    harness.shutdown().await;

    // The turn is named by the root user message that opened it.
    let root = harness
        .storage
        .load_checkpoint(harness.thread_id.as_ref())
        .await
        .expect("load checkpoint")
        .expect("root thread was checkpointed");
    let expected = root
        .messages
        .iter()
        .find_map(|entry| match &entry.message {
            Message::User(user) => Some(TurnId::from(user.message_id)),
            _ => None,
        })
        .expect("the root thread opens with a user message");

    let threads = harness.storage.all_checkpoints().await;
    assert_eq!(threads.len(), 3, "expected root + explore + probe threads");
    for checkpoint in &threads {
        assert!(!checkpoint.messages.is_empty());
        for entry in &checkpoint.messages {
            assert_eq!(
                entry.turn_id, expected,
                "{} wrote a message outside the submission's turn",
                checkpoint.agent_name
            );
        }
    }
}

/// Thread ids are derived one-way, so the parent/child structure exists only
/// implicitly unless it is written down. Each thread records who spawned it and
/// the name its own id came from, which is enough to walk the tree top-down —
/// what a fork needs, since moving a session under a new root changes every
/// derived id beneath it.
#[tokio::test]
async fn every_thread_records_how_its_parent_addressed_it() {
    // coda → explore (stateful) → probe (stateless), so the tree is two levels
    // deep and covers both derivation kinds.
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
        system_prompt: "nested-explore".into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec!["probe".into()],
    };
    let probe = AgentSpec {
        name: "probe".into(),
        description: String::new(),
        system_prompt: "explore-plain".into(),
        mode: SubAgentMode::Stateless,
        tools: vec![],
        subagents: vec![],
    };
    let mut harness = Harness::start_with_team(
        MemoryStorage::default(),
        coda,
        vec![explore, probe],
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
    .expect("timed out waiting for the root agent to finish");
    harness.shutdown().await;

    let threads = harness.storage.all_checkpoints().await;
    assert_eq!(threads.len(), 3, "expected root + explore + probe threads");

    // Exactly one thread has no parent, and it is the session's root.
    let roots: Vec<&String> = threads
        .iter()
        .filter(|c| c.parent_thread_id.is_none())
        .map(|c| &c.thread_id)
        .collect();
    assert_eq!(roots, vec![harness.thread_id.as_ref()]);

    for checkpoint in &threads {
        let Some(parent_thread_id) = &checkpoint.parent_thread_id else {
            continue;
        };
        let derivation_key = checkpoint
            .derivation_key
            .as_ref()
            .expect("a thread with a parent also records how it was derived");
        // The recorded pair is not a note about the id — it reproduces it.
        assert_eq!(
            ThreadId::from_uuid5(&ThreadId::from(parent_thread_id.clone()), derivation_key)
                .as_ref(),
            checkpoint.thread_id,
            "{} does not derive from its recorded parent",
            checkpoint.agent_name
        );
        // A stateful thread is addressed by agent name so repeat calls land on
        // it; a stateless one by the invocation, so they never do.
        match checkpoint.agent_name.as_str() {
            "explore" => assert_eq!(derivation_key, "explore"),
            "probe" => assert!(
                derivation_key.contains(':'),
                "stateless key should be the (message id, call id) pair, got {derivation_key:?}"
            ),
            other => panic!("unexpected agent {other}"),
        }
    }
}

/// Each stateless invocation must get its own thread. Deriving that thread from
/// the call id alone breaks when a provider reuses call ids — and nothing ever
/// deletes a thread's checkpoint, so the second invocation would load the first
/// one's conversation instead of starting clean. This script reuses one call id
/// across two invocations to pin that down.
#[tokio::test]
async fn stateless_invocations_reusing_a_call_id_get_separate_threads() {
    let (root, subagents) = explore_specs("twice-main", SubAgentMode::Stateless);
    let mut harness = Harness::start_with_team(
        MemoryStorage::default(),
        root,
        subagents,
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
    .expect("timed out waiting for the root agent to finish");
    harness.shutdown().await;

    let parents = tool_calling_assistants(&harness.storage, &harness.thread_id).await;
    assert_eq!(
        parents.len(),
        2,
        "the root should have called explore twice"
    );
    assert_eq!(parents[0].1, parents[1].1, "both calls reuse one call id");

    let threads: Vec<ThreadId> = parents
        .iter()
        .map(|(message_id, _)| {
            ThreadId::from_uuid5(
                &harness.thread_id,
                &MessageOrigin {
                    message_id: *message_id,
                    call_id: "call_explore".into(),
                }
                .derivation_key(),
            )
        })
        .collect();
    assert_ne!(threads[0], threads[1], "invocations shared a thread id");

    // Each thread holds exactly its own invocation: had they collided, one
    // thread would hold both opening messages and the other none.
    for thread in &threads {
        assert_eq!(
            origins_in_thread(&harness.storage, thread).await.len(),
            1,
            "a stateless invocation saw another invocation's history"
        );
    }
}

/// The assistant message that issued a call is long gone by the time an approval
/// is answered — the process may even have restarted — so its id has to be
/// persisted with the suspension. Without that, the dispatched sub-agent could
/// not record what triggered it.
#[tokio::test]
async fn subagent_dispatched_after_approval_restart_still_records_its_origin() {
    let provider = TestProvider::default();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "explore"));
    let (root, subagents) = explore_specs("main-system", SubAgentMode::Stateful);
    let team = AgentTeam::new(root, subagents).expect("valid team");
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        team.build(".", coda_tools::shared_file_locks()),
        provider.clone(),
        approval.clone(),
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
    harness.shutdown().await;

    // Captured before the restart, from the run that issued the call.
    let parents = tool_calling_assistants(&harness.storage, &harness.thread_id).await;
    let [(parent_message_id, _)] = parents.as_slice() else {
        panic!("expected exactly one tool-calling assistant message, got {parents:?}");
    };

    let mut harness = harness
        .restart(
            team.build(".", coda_tools::shared_file_locks()),
            provider,
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
    .expect("timed out waiting for completion after resume");
    harness.shutdown().await;

    let explore_thread = ThreadId::from_uuid5(&harness.thread_id, "explore");
    assert_eq!(
        origins_in_thread(&harness.storage, &explore_thread).await,
        vec![Some(MessageOrigin {
            message_id: *parent_message_id,
            call_id: "call_explore".into(),
        })]
    );
}

/// A thread waiting on a sub-agent must be able to name that sub-agent's thread
/// from its own state alone. It cannot look the child up: a child that is still
/// generating has not written a checkpoint yet, as the parked root below shows.
/// Turn cancellation rests on this — it is how an interrupted turn tells a
/// reply that is still coming from one whose producer is gone.
#[tokio::test]
async fn a_parked_thread_can_name_the_child_it_waits_on() {
    let hold = Arc::new(tokio::sync::Notify::new());
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
    let mut harness = Harness::start_with_team(
        MemoryStorage::default(),
        coda,
        vec![explore],
        TestProvider::with_hold_subagent(hold.clone()),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;

    let parked = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(checkpoint) = harness
                .storage
                .load_checkpoint(harness.thread_id.as_ref())
                .await
                .expect("load checkpoint")
                && let StoredResumePoint::ToolExecution(state) = checkpoint.resume_point
                && !state.pending_replies.is_empty()
            {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the root never parked on a pending reply");

    // The child is held mid-generation, so it has reached no write point at all.
    let stored: Vec<String> = harness
        .storage
        .all_checkpoints()
        .await
        .into_iter()
        .map(|checkpoint| checkpoint.agent_name)
        .collect();
    assert_eq!(stored, vec!["coda"], "the child should own no row yet");

    let [pending] = parked.pending_replies.as_slice() else {
        panic!(
            "expected one pending reply, got {:?}",
            parked.pending_replies
        );
    };
    // Derived from what the parent already holds — nothing here reads the child.
    let derived = ThreadId::from_uuid5(
        &harness.thread_id,
        &MessageOrigin {
            message_id: parked.parent_message_id,
            call_id: pending.call_id.clone(),
        }
        .derivation_key(),
    );

    hold.notify_waiters();
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
    .expect("timed out waiting for the root to finish");
    harness.shutdown().await;

    let child = harness
        .storage
        .all_checkpoints()
        .await
        .into_iter()
        .find(|checkpoint| checkpoint.agent_name == "explore")
        .expect("explore checkpointed once it answered");
    assert_eq!(derived.as_ref(), child.thread_id);
}
