//! SPIKE — throwaway, kept for the record. Prints what a restart can actually
//! recover about the turns that were alive when the process went away.
//!
//! To re-run: drop this file back into
//! `crates/coda_agent/src/runtime/driver_tests/`, add `mod spike;` to its
//! `mod.rs`, then
//! `cargo test -p coda_agent --lib spike -- --nocapture --test-threads=1`.
//! Conclusions are written up in FINDINGS.md next to this file.

use super::super::*;
use super::fixtures::*;
use crate::{
    Agent, AgentSpec, AgentTeam, SubAgentMode, ThreadId, ToolApprovalMode,
    persist::StoredResumePoint,
    runtime::{MemoryStorage, SessionStorage},
};
use coda_core::llm::{Message, MessageOrigin, ToolCall};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, sleep};

fn specs() -> (AgentSpec, Vec<AgentSpec>) {
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
            // Never completes until the gate is notified.
            system_prompt: "hold-subagent".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![],
            subagents: vec![],
        }],
    )
}

fn agents() -> HashMap<String, Agent> {
    let (root, subagents) = specs();
    AgentTeam::new(root, subagents)
        .expect("valid team")
        .build(".", coda_tools::shared_file_locks())
}

async fn dump(storage: &MemoryStorage, label: &str) {
    println!("\n=== {label} ===");
    for checkpoint in storage.all_checkpoints().await {
        println!(
            "thread {} agent={} parent={:?} derivation_key={:?}",
            &checkpoint.thread_id[..8],
            checkpoint.agent_name,
            checkpoint.parent_thread_id.as_deref().map(|p| &p[..8]),
            checkpoint.derivation_key,
        );
        println!(
            "  reply_target={:?}",
            checkpoint
                .reply_target
                .as_ref()
                .map(|t| (&t.sender_name, &t.call_id))
        );
        match &checkpoint.resume_point {
            StoredResumePoint::Generation => println!("  resume_point=Generation"),
            StoredResumePoint::ToolExecution(state) => println!(
                "  resume_point=ToolExecution parent_message_id={} pending_replies={:?} tool_calls={}",
                state.parent_message_id,
                state
                    .pending_replies
                    .iter()
                    .map(|r| (&r.call_id, &r.tool_name))
                    .collect::<Vec<_>>(),
                state.tool_calls.len(),
            ),
            StoredResumePoint::PendingApproval { .. } => println!("  resume_point=PendingApproval"),
        }
        println!(
            "  turns in history: {:?}",
            checkpoint
                .messages
                .iter()
                .map(|entry| format!(
                    "{}/{}",
                    &entry.turn_id.to_string()[..8],
                    match &entry.message {
                        Message::User(_) => "user",
                        Message::Assistant(_) => "assistant",
                        Message::Tool(_) => "tool",
                        Message::System(_) => "system",
                    }
                ))
                .collect::<Vec<_>>()
        );
    }
}

/// Q1 + Q3, hard-crash flavour: the process dies while a sub-agent call is in
/// flight, so nothing was drained and no snapshot was written.
#[tokio::test]
async fn spike_hard_crash_while_subagent_in_flight() {
    let storage = MemoryStorage::default();
    let gate = Arc::new(Notify::new());
    let (root, subagents) = specs();
    let mut harness = Harness::start_with_team(
        storage.clone(),
        root,
        subagents,
        TestProvider::with_hold_subagent(gate.clone()),
        ToolApprovalMode::Auto,
        "t1",
    )
    .await;

    // Wait until explore is actually generating, i.e. coda has dispatched and
    // gone idle holding a pending reply.
    loop {
        let (name, _, event) = harness.next_event().await;
        if name == "explore" && matches!(event, AgentEvent::LLMStart(_)) {
            break;
        }
    }
    sleep(Duration::from_millis(50)).await;

    dump(&storage, "hard crash: what survives").await;

    let snapshot = storage
        .load_session_snapshot(harness.thread_id.as_ref())
        .await
        .expect("load snapshot");
    println!("\nsnapshot present? {}", snapshot.is_some());

    // Can the parent name its child's thread from its own checkpoint alone?
    let coda = storage
        .all_checkpoints()
        .await
        .into_iter()
        .find(|c| c.agent_name == "coda")
        .expect("coda checkpoint");
    if let StoredResumePoint::ToolExecution(state) = &coda.resume_point {
        for pending in &state.pending_replies {
            let derivation_key = MessageOrigin {
                message_id: state.parent_message_id,
                call_id: pending.call_id.clone(),
            }
            .derivation_key();
            let derived =
                ThreadId::from_uuid5(&ThreadId::from(coda.thread_id.clone()), &derivation_key);
            let actual = storage
                .all_checkpoints()
                .await
                .into_iter()
                .find(|c| c.agent_name == "explore")
                .map(|c| c.thread_id);
            println!(
                "\npending reply {} -> derived child thread {}  actual explore thread {:?}  match={}",
                pending.call_id,
                &derived.as_ref()[..8],
                actual.as_deref().map(|t| &t[..8]),
                Some(derived.as_ref()) == actual.as_deref(),
            );
        }
    }

    gate.notify_waiters();
}

/// Q1, graceful flavour: a task submitted during the exit drain. What order does
/// the snapshot preserve, and does the replayed envelope carry its turn id?
#[tokio::test]
async fn spike_graceful_exit_with_a_queued_task() {
    let storage = MemoryStorage::default();
    let gate = Arc::new(Notify::new());
    let (root, subagents) = specs();
    let mut harness = Harness::start_with_team(
        storage.clone(),
        root,
        subagents,
        TestProvider::with_hold_subagent(gate.clone()),
        ToolApprovalMode::Auto,
        "t1",
    )
    .await;

    loop {
        let (name, _, event) = harness.next_event().await;
        if name == "explore" && matches!(event, AgentEvent::LLMStart(_)) {
            break;
        }
    }
    sleep(Duration::from_millis(50)).await;

    harness.runtime.request_exit().await;
    // Now the barrier is set, so this one is buffered into the snapshot rather
    // than delivered.
    harness.send_task("t2").await;
    let _ = harness
        .runtime
        .wait_for_exit(Some(Duration::from_millis(300)))
        .await;

    let snapshot = storage
        .load_session_snapshot(harness.thread_id.as_ref())
        .await
        .expect("load snapshot")
        .expect("snapshot written on exit");
    println!("\n=== graceful exit snapshot ===");
    println!("active_threads: {:?}", snapshot.active_threads);
    for (agent, envelopes) in &snapshot.agent_drained_envelopes {
        println!(
            "agent_drained[{agent}]: {:?}",
            envelopes.iter().map(|e| &e.body).collect::<Vec<_>>()
        );
    }
    for (agent, envelopes) in &snapshot.drained_envelopes {
        println!(
            "drained[{agent}]: {:?}",
            envelopes.iter().map(|e| &e.body).collect::<Vec<_>>()
        );
    }

    dump(&storage, "graceful exit: checkpoints").await;

    gate.notify_waiters();
}

/// Is the stored snapshot consumed once, or can a second restart replay it
/// again? Decides whether a recovered work list converges or duplicates.
#[tokio::test]
async fn spike_snapshot_replayed_twice() {
    let storage = MemoryStorage::default();
    let gate = Arc::new(Notify::new());
    let (root, subagents) = specs();
    let harness = Harness::start_with_team(
        storage.clone(),
        root,
        subagents,
        TestProvider::with_hold_subagent(gate.clone()),
        ToolApprovalMode::Auto,
        "t1",
    )
    .await;

    sleep(Duration::from_millis(50)).await;
    harness.runtime.request_exit().await;
    harness.send_task("queued").await;
    let _ = harness
        .runtime
        .wait_for_exit(Some(Duration::from_millis(300)))
        .await;

    let before = storage
        .load_session_snapshot(harness.thread_id.as_ref())
        .await
        .expect("load")
        .expect("snapshot");
    println!(
        "\nbefore first restart: drained={:?}",
        before
            .drained_envelopes
            .iter()
            .map(|(k, v)| (k, v.len()))
            .collect::<Vec<_>>()
    );

    let restarted = harness
        .restart(
            agents(),
            TestProvider::with_hold_subagent(gate.clone()),
            ToolApprovalMode::Auto,
            HashMap::new(),
        )
        .await;
    sleep(Duration::from_millis(100)).await;

    let after = storage
        .load_session_snapshot(restarted.thread_id.as_ref())
        .await
        .expect("load")
        .expect("snapshot");
    println!(
        "after first restart (no graceful exit): drained={:?} agent_drained={:?}",
        after
            .drained_envelopes
            .iter()
            .map(|(k, v)| (k, v.len()))
            .collect::<Vec<_>>(),
        after
            .agent_drained_envelopes
            .iter()
            .map(|(k, v)| (k, v.len()))
            .collect::<Vec<_>>()
    );

    gate.notify_waiters();
}

/// Q2 + Q3, restart-resume flavour: the sub-agent suspends for approval, so it
/// *does* checkpoint and *does* land in `active_threads`. Does the whole
/// sub-tree of one turn share a single turn id?
#[tokio::test]
async fn spike_subagent_suspended_for_approval() {
    let storage = MemoryStorage::default();
    let (root, subagents) = explore_read_todos_specs("main-system");
    let harness = Harness::start_with_team(
        storage.clone(),
        root,
        subagents,
        TestProvider::default(),
        ToolApprovalMode::RequireWhen(Arc::new(|call: &ToolCall| call.name == "read_todos")),
        "t1",
    )
    .await;

    sleep(Duration::from_millis(150)).await;
    harness.runtime.request_exit().await;
    let _ = harness
        .runtime
        .wait_for_exit(Some(Duration::from_millis(500)))
        .await;

    let snapshot = storage
        .load_session_snapshot(harness.thread_id.as_ref())
        .await
        .expect("load")
        .expect("snapshot");
    println!("\n=== subagent suspended for approval ===");
    println!("active_threads: {:?}", snapshot.active_threads);
    dump(&storage, "checkpoints").await;
}

/// Q1, ordering: two tasks queued behind the barrier. Is their order preserved,
/// and can each one's turn id be read straight off the replayed envelope?
#[tokio::test]
async fn spike_queued_task_order() {
    let storage = MemoryStorage::default();
    let gate = Arc::new(Notify::new());
    let (root, subagents) = specs();
    let harness = Harness::start_with_team(
        storage.clone(),
        root,
        subagents,
        TestProvider::with_hold_subagent(gate.clone()),
        ToolApprovalMode::Auto,
        "t1",
    )
    .await;

    sleep(Duration::from_millis(50)).await;
    harness.runtime.request_exit().await;
    harness.send_task("t2").await;
    harness.send_task("t3").await;
    let _ = harness
        .runtime
        .wait_for_exit(Some(Duration::from_millis(300)))
        .await;

    let snapshot = storage
        .load_session_snapshot(harness.thread_id.as_ref())
        .await
        .expect("load")
        .expect("snapshot");
    println!("\n=== queued task order ===");
    for (agent, envelopes) in &snapshot.drained_envelopes {
        for envelope in envelopes {
            println!("drained[{agent}]: {:?}", envelope.body);
        }
    }

    gate.notify_waiters();
}
