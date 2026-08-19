//! Auto-compaction: triggered mid-turn on the root thread only, protecting
//! the turn in progress, compacting at most once per turn on success, and
//! retrying on a later over-threshold check after a failed attempt.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, RunConfig, SubAgentMode, ToolApprovalMode,
    runtime::{MemoryStorage, SessionStorage},
};
use coda_core::llm::Message;
use coda_tools::ReadTodosToolSpec;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::time::{Duration, timeout};

fn config_with_threshold(provider: TestProvider, threshold: u32) -> RunConfig<TestProvider> {
    let mut config = test_config(provider, ToolApprovalMode::Auto);
    config.default_model.auto_compact_threshold_tokens = threshold;
    config
}

fn coda_spec(system_prompt: &str, subagents: Vec<String>) -> AgentSpec {
    AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: system_prompt.into(),
        mode: SubAgentMode::Stateful,
        tools: if subagents.is_empty() {
            vec![Box::new(ReadTodosToolSpec)]
        } else {
            vec![]
        },
        subagents,
    }
}

/// A short, order-preserving label for a history entry — enough to assert on
/// sequence and kind without pattern-matching every field at every call site.
fn labels<'a>(entries: impl IntoIterator<Item = &'a HistoryEntry>) -> Vec<String> {
    entries
        .into_iter()
        .map(|entry| match &entry.message {
            Message::User(user) => format!("user:{}", user.first_text().unwrap_or_default()),
            Message::Assistant(assistant) if assistant.tool_calls.is_empty() => {
                format!("assistant:{}", assistant.content)
            }
            Message::Assistant(assistant) => format!(
                "assistant-calls:{}",
                assistant
                    .tool_calls
                    .iter()
                    .map(|call| call.id.clone())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Message::Tool(tool) => format!("tool:{}", tool.id),
            Message::Custom(custom) => format!("custom:{}", custom.kind),
        })
        .collect()
}

/// Waits for the root's final answer — an `LLMEnd` with no tool calls whose
/// content is `expected` — so a test can drive one turn to completion before
/// sending the next.
async fn wait_for_root_answer(harness: &mut Harness<MemoryStorage>, expected: &str) {
    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::LLMEnd(msg)) = (agent_name.as_str(), event)
                && msg.tool_calls.is_empty()
            {
                assert_eq!(msg.content, expected);
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the root to answer");
}

/// The flagship scenario: a long-running "second" turn crosses the threshold
/// mid-flight (after its first tool call), auto-compacts the *previous* turn
/// while leaving everything the current turn already produced untouched, and
/// does not compact a second time even though usage stays over threshold for
/// the rest of the turn.
#[tokio::test]
async fn mid_turn_auto_compaction_protects_the_current_turn_and_compacts_once() {
    let config = config_with_threshold(TestProvider::default(), 1_000);
    let agents = AgentTeam::new(coda_spec("auto-compact-main", vec![]), vec![])
        .expect("valid team")
        .build(".", coda_tools::shared_file_locks());
    let mut harness =
        Harness::start_with_config(MemoryStorage::default(), agents, config, "first").await;
    wait_for_root_answer(&mut harness, "first done").await;

    harness.send_task("second").await;
    wait_for_root_answer(&mut harness, "second done").await;
    harness.shutdown().await;

    let history = harness
        .storage
        .load_checkpoint(harness.thread_id.as_ref())
        .await
        .expect("load checkpoint")
        .expect("checkpoint exists")
        .messages;

    let summaries: Vec<_> = history
        .iter()
        .filter(|entry| matches!(&entry.message, Message::Custom(custom) if custom.kind == message_view::COMPACTION_KIND))
        .collect();
    assert_eq!(
        summaries.len(),
        1,
        "compaction should not repeat once it has succeeded for this turn: {:?}",
        labels(&history)
    );
    assert!(
        !history.iter().any(|entry| matches!(&entry.message, Message::Custom(custom) if custom.kind == message_view::COMPACTION_FAILED_KIND)),
        "the scripted summary always succeeds in this test"
    );

    let first_done = history
        .iter()
        .find(|entry| matches!(&entry.message, Message::Assistant(a) if a.content == "first done"))
        .expect("turn 1's answer");
    let second_user = history
        .iter()
        .find(
            |entry| matches!(&entry.message, Message::User(u) if u.first_text() == Some("second")),
        )
        .expect("turn 2's opening message");
    let Message::Custom(summary) = &summaries[0].message else {
        unreachable!("filtered to Custom above");
    };
    assert_eq!(
        summary.cutoff,
        Some(first_done.message.message_id()),
        "the summary should cover exactly through turn 1's last message"
    );
    assert_eq!(
        summaries[0].turn_id, second_user.turn_id,
        "the summary is appended during turn 2, so it carries turn 2's tag, \
         even though its cutoff protects turn 2's own content"
    );

    // The model's view leads with the summary, then shows every one of turn
    // 2's own messages in original order — reordered ahead of nothing it
    // wasn't already behind, and with none of them lost to the boundary.
    assert_eq!(
        labels(message_view::model_view(&history)),
        vec![
            "custom:compaction".to_string(),
            "user:second".to_string(),
            "assistant-calls:call_1".to_string(),
            "tool:call_1".to_string(),
            "assistant-calls:call_2".to_string(),
            "tool:call_2".to_string(),
            "assistant:second done".to_string(),
        ]
    );
}

/// Auto-compaction never runs on a sub-agent thread. `explore` is invoked
/// once per root turn and is itself stateful, so its own history carries two
/// turn tags by its second invocation — a legal compaction target if the
/// root-thread guard were missing, since its second invocation goes over
/// threshold too.
#[tokio::test]
async fn auto_compaction_never_runs_on_a_subagent_thread() {
    let config = config_with_threshold(TestProvider::default(), 1_000);
    let root = coda_spec("auto-compact-subagent-main", vec!["explore".into()]);
    let explore = AgentSpec {
        name: "explore".into(),
        description: String::new(),
        system_prompt: "auto-compact-subagent-explore".into(),
        mode: SubAgentMode::Stateful,
        tools: vec![Box::new(ReadTodosToolSpec)],
        subagents: vec![],
    };
    let agents = AgentTeam::new(root, vec![explore])
        .expect("valid team")
        .build(".", coda_tools::shared_file_locks());
    let mut harness =
        Harness::start_with_config(MemoryStorage::default(), agents, config, "first").await;
    wait_for_root_answer(&mut harness, "first done").await;

    harness.send_task("second").await;
    wait_for_root_answer(&mut harness, "second done").await;
    harness.shutdown().await;

    let explore_thread = ThreadId::from_uuid5(&harness.thread_id, "explore");
    let explore_history = harness
        .storage
        .load_checkpoint(explore_thread.as_ref())
        .await
        .expect("load explore's checkpoint")
        .expect("explore's checkpoint exists")
        .messages;
    assert!(
        !explore_history
            .iter()
            .any(|entry| matches!(&entry.message, Message::Custom(_))),
        "explore's own over-threshold second invocation must not have \
         attempted a compaction of any kind: {:?}",
        labels(&explore_history)
    );
}

/// A failed attempt does not move the boundary, so the next over-threshold
/// check in the same turn retries against the same target rather than being
/// permanently suppressed.
#[tokio::test]
async fn a_failed_attempt_is_retried_at_the_next_check_in_the_same_turn() {
    let fail_once = Arc::new(AtomicBool::new(true));
    let config = config_with_threshold(
        TestProvider::with_fail_next_compaction(fail_once.clone()),
        1_000,
    );
    let agents = AgentTeam::new(coda_spec("auto-compact-main", vec![]), vec![])
        .expect("valid team")
        .build(".", coda_tools::shared_file_locks());
    let mut harness =
        Harness::start_with_config(MemoryStorage::default(), agents, config, "first").await;
    wait_for_root_answer(&mut harness, "first done").await;

    harness.send_task("second").await;
    wait_for_root_answer(&mut harness, "second done").await;
    harness.shutdown().await;

    assert!(
        !fail_once.load(Ordering::SeqCst),
        "the scripted failure should have been consumed by the first attempt"
    );

    let history = harness
        .storage
        .load_checkpoint(harness.thread_id.as_ref())
        .await
        .expect("load checkpoint")
        .expect("checkpoint exists")
        .messages;
    let failures = history
        .iter()
        .filter(|entry| matches!(&entry.message, Message::Custom(custom) if custom.kind == message_view::COMPACTION_FAILED_KIND))
        .count();
    let successes = history
        .iter()
        .filter(|entry| matches!(&entry.message, Message::Custom(custom) if custom.kind == message_view::COMPACTION_KIND))
        .count();
    assert_eq!(failures, 1, "the first attempt was scripted to fail");
    assert_eq!(
        successes,
        1,
        "the retry at the next check should have succeeded: {:?}",
        labels(&history)
    );
}

/// A compaction can succeed and then the *real* generation right after it —
/// the one it made room for — can still fail on its own (a provider error,
/// unrelated to the compaction). The turn ends on that error, not on the
/// compaction. A later turn must not re-attempt the compaction (the summary
/// is already that turn's last message, so `compaction::cutoff` sees nothing
/// new) and must build its request from the already-compacted view rather
/// than the stale, over-threshold one the failed turn saw.
#[tokio::test]
async fn a_compaction_survives_the_generation_that_failed_right_after_it() {
    let config = config_with_threshold(TestProvider::default(), 1_000);
    let agents = AgentTeam::new(
        coda_spec("auto-compact-fail-then-continue-main", vec![]),
        vec![],
    )
    .expect("valid team")
    .build(".", coda_tools::shared_file_locks());
    let mut harness =
        Harness::start_with_config(MemoryStorage::default(), agents, config, "first").await;
    wait_for_root_answer(&mut harness, "first done").await;

    harness.send_task("second").await;
    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::Error(_)) = (agent_name.as_str(), event) {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for turn 2 to end in error");

    harness.send_task("third").await;
    let third_request = timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if let ("coda", AgentEvent::LLMStart(request)) = (agent_name.as_str(), event) {
                return request;
            }
        }
    })
    .await
    .expect("timed out waiting for turn 3's request");
    wait_for_root_answer(&mut harness, "third done").await;
    harness.shutdown().await;

    // Turn 3 sees the compacted view, not the raw pre-compaction history —
    // the failed generation's own request (never captured here) would have
    // been the one built from the full, over-threshold history; this one,
    // built after the compaction that already happened, must not repeat it.
    let sent = format!("{third_request:?}");
    assert!(
        sent.contains("gist of the earlier turn"),
        "turn 3's request should carry the summary: {sent}"
    );
    assert!(
        !sent.contains("first done"),
        "turn 3's request should not carry turn 1's raw content past the \
         summary that replaced it: {sent}"
    );

    let history = harness
        .storage
        .load_checkpoint(harness.thread_id.as_ref())
        .await
        .expect("load checkpoint")
        .expect("checkpoint exists")
        .messages;
    let summaries = history
        .iter()
        .filter(|entry| matches!(&entry.message, Message::Custom(custom) if custom.kind == message_view::COMPACTION_KIND))
        .count();
    assert_eq!(
        summaries,
        1,
        "turn 3 must not have attempted a second compaction: {:?}",
        labels(&history)
    );
}
