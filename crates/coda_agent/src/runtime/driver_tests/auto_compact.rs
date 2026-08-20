//! Auto-compaction: triggered mid-turn on any thread, protecting the turn in
//! progress, compacting at most once per turn on success, and retrying on a
//! later over-threshold check after a failed attempt.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, RunConfig, StoredCheckpoint, SubAgentMode, ToolApprovalMode,
    agent::HistoryEntry,
    runtime::{MemoryStorage, SessionStorage, StoredResumePoint},
};
use coda_core::llm::{
    CompletionUsage, Message, MessageId, ToolCall, ToolCallOutcome, ToolMessage, ToolOutput,
    UserMessage,
};
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

#[tokio::test]
async fn a_first_turn_can_compact_after_its_completed_tool_batch() {
    let config = config_with_threshold(TestProvider::default(), 1_000);
    let agents = AgentTeam::new(coda_spec("auto-compact-first-turn-main", vec![]), vec![])
        .expect("valid team")
        .build(".", coda_tools::shared_file_locks());
    let mut harness =
        Harness::start_with_config(MemoryStorage::default(), agents, config, "only").await;
    wait_for_root_answer(&mut harness, "only done").await;
    harness.shutdown().await;

    let history = harness
        .storage
        .load_checkpoint(harness.thread_id.as_ref())
        .await
        .expect("load checkpoint")
        .expect("checkpoint exists")
        .messages;
    let result = history
        .iter()
        .find(|entry| matches!(&entry.message, Message::Tool(tool) if tool.id == "call_first"))
        .expect("first-turn tool result");
    let summary = history
        .iter()
        .find_map(|entry| match &entry.message {
            Message::Custom(custom) if custom.kind == message_view::COMPACTION_KIND => Some(custom),
            _ => None,
        })
        .expect("first-turn compaction");
    assert_eq!(summary.cutoff, Some(result.message.message_id()));
    assert_eq!(
        labels(message_view::model_view(&history)),
        ["custom:compaction", "assistant:only done"]
    );
}

fn malformed_history(usage: Option<CompletionUsage>) -> Vec<HistoryEntry> {
    let turn = TurnId::from(MessageId::new());
    let assistant = Message::Assistant(coda_core::llm::AssistantMessage {
        tool_calls: vec![
            ToolCall {
                id: "finished".to_string(),
                name: "read_todos".to_string(),
                arguments: Some("{}".to_string()),
            },
            ToolCall {
                id: "missing".to_string(),
                name: "read_todos".to_string(),
                arguments: Some("{}".to_string()),
            },
        ],
        usage,
        ..assistant()
    });
    vec![
        HistoryEntry {
            turn_id: turn,
            message: Message::User(UserMessage::text(MessageId::new(), "old task")),
        },
        HistoryEntry {
            turn_id: turn,
            message: assistant,
        },
        HistoryEntry {
            turn_id: turn,
            message: Message::Tool(ToolMessage::new(
                "finished",
                "read_todos",
                ToolOutput::Ok("done".to_string()),
                ToolCallOutcome::Auto,
                None,
            )),
        },
    ]
}

#[tokio::test]
async fn malformed_history_never_reaches_llm_start_on_usage_fast_paths() {
    for usage in [
        None,
        Some(CompletionUsage {
            total_tokens: 100,
            ..Default::default()
        }),
    ] {
        let config = config_with_threshold(TestProvider::default(), 1_000);
        let storage = MemoryStorage::default();
        let thread_id = ThreadId::new();
        storage
            .save_checkpoint(
                thread_id.as_ref().to_string(),
                StoredCheckpoint {
                    thread_id: thread_id.as_ref().to_string(),
                    agent_name: "coda".to_string(),
                    parent_thread_id: None,
                    derivation_key: None,
                    reply_target: None,
                    messages: malformed_history(usage),
                    state: vec![],
                    resume_point: StoredResumePoint::Generation,
                    suspended_at: jiff::Timestamp::default(),
                },
            )
            .await
            .expect("seed malformed checkpoint");
        let agents = AgentTeam::new(coda_spec("main-system", vec![]), vec![])
            .expect("valid team")
            .build(".", coda_tools::shared_file_locks());
        let mut harness =
            Harness::start_with_config_at(storage, agents, config, thread_id, "new task").await;

        timeout(Duration::from_secs(2), async {
            loop {
                let (agent_name, _, event) = harness.next_event().await;
                if agent_name != "coda" {
                    continue;
                }
                match event {
                    AgentEvent::LLMStart(request) => {
                        panic!("malformed history reached the provider: {request:?}")
                    }
                    AgentEvent::Error(error) => {
                        assert!(error.contains("missing results"), "{error}");
                        return;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for invalid-history error");
        harness.shutdown().await;
    }
}

/// A "second" turn first compacts the previous turn, then uses an intra-turn
/// boundary when its actual post-compaction usage remains over threshold.
#[tokio::test]
async fn mid_turn_auto_compaction_prefers_a_turn_then_falls_back_inside_it() {
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
        2,
        "the second over-threshold check should compact new current-turn work: {:?}",
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
    let Message::Custom(first_summary) = &summaries[0].message else {
        unreachable!("filtered to Custom above");
    };
    assert_eq!(
        first_summary.cutoff,
        Some(first_done.message.message_id()),
        "the summary should cover exactly through turn 1's last message"
    );
    assert_eq!(
        summaries[0].turn_id, second_user.turn_id,
        "the summary is appended during turn 2, so it carries turn 2's tag, \
        even though its cutoff protects turn 2's own content"
    );

    let call_2_result = history
        .iter()
        .find(|entry| matches!(&entry.message, Message::Tool(tool) if tool.id == "call_2"))
        .expect("turn 2's second tool result");
    let Message::Custom(second_summary) = &summaries[1].message else {
        unreachable!("filtered to Custom above");
    };
    assert_eq!(
        second_summary.cutoff,
        Some(call_2_result.message.message_id()),
        "the fallback should cover the latest complete tool batch"
    );

    // The second summary replaces all completed work; only the final answer
    // produced after it remains raw.
    assert_eq!(
        labels(message_view::model_view(&history)),
        vec![
            "custom:compaction".to_string(),
            "assistant:second done".to_string(),
        ]
    );
}

/// A live client sees `CompactionStart` before the summary/failure it
/// precedes — the cue an attached UI shows something is happening, rather
/// than the result just appearing.
#[tokio::test]
async fn auto_compaction_emits_a_start_event_before_the_result() {
    let config = config_with_threshold(TestProvider::default(), 1_000);
    let agents = AgentTeam::new(coda_spec("auto-compact-main", vec![]), vec![])
        .expect("valid team")
        .build(".", coda_tools::shared_file_locks());
    let mut harness =
        Harness::start_with_config(MemoryStorage::default(), agents, config, "first").await;
    wait_for_root_answer(&mut harness, "first done").await;

    harness.send_task("second").await;
    timeout(Duration::from_secs(2), async {
        let mut saw_start = false;
        loop {
            let (agent_name, _, event) = harness.next_event().await;
            if agent_name != "coda" {
                continue;
            }
            match event {
                AgentEvent::CompactionStart => saw_start = true,
                AgentEvent::Custom(_) => {
                    assert!(saw_start, "the start event should precede the result");
                    return;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for the compaction events");
    wait_for_root_answer(&mut harness, "second done").await;
    harness.shutdown().await;
}

/// Auto-compaction runs on a sub-agent thread exactly the same way it runs on
/// the root: `explore` is stateful and invoked once per root turn, so its own
/// history carries two turn tags by its second invocation — its second
/// invocation crosses threshold after its own tool call, compacting through
/// its first invocation's answer and protecting the second's own content.
#[tokio::test]
async fn auto_compaction_runs_on_a_subagent_thread_too() {
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

    let summaries: Vec<_> = explore_history
        .iter()
        .filter(|entry| matches!(&entry.message, Message::Custom(custom) if custom.kind == message_view::COMPACTION_KIND))
        .collect();
    assert_eq!(
        summaries.len(),
        1,
        "explore's own over-threshold second invocation should have compacted \
         exactly once: {:?}",
        labels(&explore_history)
    );

    let round_1_done = explore_history
        .iter()
        .find(|entry| matches!(&entry.message, Message::Assistant(a) if a.content == "explore round 1 done"))
        .expect("explore's first invocation answer");
    let Message::Custom(summary) = &summaries[0].message else {
        unreachable!("filtered to Custom above");
    };
    assert_eq!(
        summary.cutoff,
        Some(round_1_done.message.message_id()),
        "the summary should cover exactly through explore's first invocation"
    );

    let view_labels = labels(message_view::model_view(&explore_history));
    assert_eq!(view_labels.first(), Some(&"custom:compaction".to_string()));
    assert_eq!(
        view_labels.last(),
        Some(&"assistant:explore round 2 done".to_string()),
        "explore's own second-invocation content must survive the boundary \
         its own compaction created: {view_labels:?}"
    );
}

/// A failed attempt doesn't move the boundary, so the next over-threshold
/// check in the same turn retries against the same target.
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

/// A compaction can succeed and then the generation it made room for can
/// still fail on its own (a provider error, unrelated to the compaction). A
/// later turn compacts the failed turn's newly retained work, then builds its
/// request from the newer compacted view rather than stale raw history.
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

    // Turn 3 sees the compacted view, not the raw pre-compaction history.
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
        2,
        "turn 3 should compact the failed turn's work as a new prefix: {:?}",
        labels(&history)
    );
}
