//! Which thread a sub-agent invocation runs in, and what it records about
//! who called it: stateful vs. stateless derivation, reused call ids, nested
//! delegation, and turn tagging across threads.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, SubAgentMode, ToolApprovalMode, ToolCallResolution,
    runtime::{MemoryStorage, SessionStorage},
};
use coda_core::llm::{Message, MessageId, MessageOrigin, ToolCallOutcome, ToolOutput, TurnId};
use coda_tools::ReadTodosToolSpec;
use std::collections::{HashMap, HashSet};
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
                            &pending.agent_name,
                            &pending.thread_id,
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

/// When a new task pre-empts calls awaiting approval, the driver writes those
/// calls off as aborted results. Those results answer the *previous* turn's
/// assistant message, so they have to stay with the previous turn: were they
/// tagged with the arriving one, rewinding to it would delete them and leave
/// tool calls with no results — history a provider rejects outright. This is why
/// the turn advances when the user message is appended rather than when the
/// envelope arrives.
#[tokio::test]
async fn preempted_calls_are_written_off_under_the_turn_they_belonged_to() {
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
    let mut harness = Harness::start_agents(
        MemoryStorage::default(),
        team.build(".", coda_tools::shared_file_locks()),
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
    .expect("timed out waiting for suspension");

    // A new task instead of a resume: the pending call gets discarded.
    harness.send_task("phase1").await;
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
    .expect("timed out waiting for the pre-empting turn to finish");
    harness.shutdown().await;

    let history = harness
        .storage
        .load_checkpoint(harness.thread_id.as_ref())
        .await
        .expect("load checkpoint")
        .expect("root thread was checkpointed")
        .messages;

    let turns: Vec<TurnId> = history
        .iter()
        .filter_map(|entry| match &entry.message {
            Message::User(user) => Some(TurnId::from(user.message_id)),
            _ => None,
        })
        .collect();
    let [first_turn, second_turn] = turns.as_slice() else {
        panic!("expected two user messages, got {}", turns.len());
    };

    let discarded = history
        .iter()
        .find(|entry| {
            matches!(&entry.message, Message::Tool(tool)
                if tool.id == "call_approve" && matches!(tool.outcome, ToolCallOutcome::Aborted))
        })
        .expect("the pre-empted call was written off");
    assert_eq!(
        discarded.turn_id, *first_turn,
        "the write-off was attributed to the turn that pre-empted it"
    );

    // Rewind to the second turn and check the survivors are still well formed:
    // every remaining tool call has its result.
    let kept: Vec<&Message> = history
        .iter()
        .filter(|entry| entry.turn_id != *second_turn)
        .map(|entry| &entry.message)
        .collect();
    let answered: HashSet<&str> = kept
        .iter()
        .filter_map(|message| match message {
            Message::Tool(tool) => Some(tool.id.as_str()),
            _ => None,
        })
        .collect();
    for message in &kept {
        if let Message::Assistant(assistant) = message {
            for call in &assistant.tool_calls {
                assert!(
                    answered.contains(call.id.as_str()),
                    "truncating the later turn left {} unanswered",
                    call.id
                );
            }
        }
    }
    assert!(!answered.is_empty(), "nothing survived the truncation");
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
