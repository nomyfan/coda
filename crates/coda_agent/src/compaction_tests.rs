use super::*;
use coda_core::llm::{
    AssistantMessage, CompletionUsage, RequestMessage, ToolCall, ToolCallOutcome, ToolMessage,
};

fn entry(message: Message) -> HistoryEntry {
    HistoryEntry::new(TurnId::from(MessageId::new()), message)
}

fn entry_in(turn: TurnId, message: Message) -> HistoryEntry {
    HistoryEntry::new(turn, message)
}

fn user(text: &str) -> HistoryEntry {
    entry(Message::User(UserMessage::text(MessageId::new(), text)))
}

fn user_in(turn: TurnId, text: &str) -> HistoryEntry {
    entry_in(
        turn,
        Message::User(UserMessage::text(MessageId::new(), text)),
    )
}

fn assistant_in(turn: TurnId, call_id: &str, prompt_tokens: Option<u32>) -> HistoryEntry {
    entry_in(
        turn,
        Message::Assistant(AssistantMessage {
            message_id: MessageId::new(),
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: call_id.to_string(),
                name: "tool".to_string(),
                arguments: Some("{}".to_string()),
            }],
            usage: prompt_tokens.map(|prompt_tokens| CompletionUsage {
                prompt_tokens,
                total_tokens: prompt_tokens,
                ..Default::default()
            }),
            reasoning_content: None,
            reasoning_continuation: None,
            reasoning_ended_at: None,
            aborted: false,
            started_at: jiff::Timestamp::default(),
            ended_at: jiff::Timestamp::default(),
        }),
    )
}

fn tool_in(turn: TurnId, call_id: &str) -> HistoryEntry {
    entry_in(
        turn,
        Message::Tool(ToolMessage::new(
            call_id,
            "tool",
            ToolOutput::Ok("done".to_string()),
            ToolCallOutcome::Auto,
            None,
        )),
    )
}

fn planned_cutoff(
    history: &[HistoryEntry],
    prefer_before_turn: Option<TurnId>,
    protect_from: Option<MessageId>,
    threshold: Option<u32>,
) -> Option<Cutoff> {
    cutoff(history, prefer_before_turn, protect_from, threshold).expect("valid history")
}

fn request_text(request: &ChatCompletionRequest) -> String {
    match &request.messages[1] {
        RequestMessage::User(user) => user.first_text().unwrap_or_default().to_string(),
        _ => panic!("the task is the second message"),
    }
}

#[test]
fn the_transcript_carries_calls_and_their_results() {
    let request = summary_request(
        "some-model".into(),
        None,
        None,
        &[
            entry(Message::User(UserMessage::text(
                MessageId::new(),
                "read the config",
            ))),
            entry(Message::Assistant(AssistantMessage {
                message_id: MessageId::new(),
                content: "Looking.".into(),
                tool_calls: vec![ToolCall {
                    id: "call_read".into(),
                    name: "read_file".into(),
                    arguments: Some(r#"{"file_path":"coda-server.toml"}"#.into()),
                }],
                usage: None,
                reasoning_content: None,
                reasoning_continuation: None,
                reasoning_ended_at: None,
                aborted: false,
                started_at: jiff::Timestamp::default(),
                ended_at: jiff::Timestamp::default(),
            })),
            entry(Message::Tool(ToolMessage::new(
                "call_read",
                "read_file",
                ToolOutput::Ok("[database]".into()),
                ToolCallOutcome::Auto,
                None,
            ))),
        ],
        "",
    );

    let text = request_text(&request);
    assert!(text.contains("read the config"));
    assert!(text.contains("### Tool call: read_file {\"file_path\":\"coda-server.toml\"}"));
    assert!(text.contains("### Tool result: read_file"));
    assert!(text.contains("[database]"));
}

/// One instruction over a record, not a conversation — no tools, no orphan
/// tool results.
#[test]
fn the_summary_request_replays_nothing() {
    let request = summary_request(
        "some-model".into(),
        Some(1024),
        None,
        &[entry(Message::Tool(ToolMessage::new(
            "call_orphan",
            "shell",
            ToolOutput::Err("boom".into()),
            ToolCallOutcome::Auto,
            None,
        )))],
        "",
    );

    assert!(request.tools.is_empty());
    assert_eq!(request.messages.len(), 2);
    assert!(matches!(request.messages[0], RequestMessage::System(_)));
    assert!(request_text(&request).contains("### Tool error: shell"));
}

#[test]
fn user_instructions_reach_both_the_request_and_the_summary() {
    let request = summary_request(
        "m".into(),
        None,
        None,
        &[],
        "keep the architecture decisions",
    );
    let text = request_text(&request);
    let instructions_at = text
        .find("keep the architecture decisions")
        .expect("instructions");
    let transcript_at = text.find("<transcript>").expect("transcript");
    assert!(
        instructions_at < transcript_at,
        "instructions must precede the transcript so a truncated window cannot drop them"
    );

    let cutoff_id = MessageId::new();
    let Message::Compaction(summary) = summary_message(
        cutoff_id,
        Trigger::Manual {
            instructions: "keep the architecture decisions",
        },
        "the gist",
    ) else {
        panic!("a summary is a compaction message");
    };
    assert_eq!(
        summary.outcome,
        CompactionOutcome::Summary { cutoff: cutoff_id }
    );
    assert!(summary.content.contains("keep the architecture decisions"));
    assert!(summary.content.contains("the gist"));
}

#[test]
fn a_bare_compact_adds_no_instruction_framing() {
    let Message::User(command) = command_message("") else {
        panic!("the command is a user message");
    };
    assert_eq!(command.first_text(), Some("/compact"));

    let cutoff_id = MessageId::new();
    let Message::Compaction(summary) =
        summary_message(cutoff_id, Trigger::Manual { instructions: "" }, "the gist")
    else {
        panic!("a summary is a compaction message");
    };
    assert_eq!(summary.content, "the gist");
    assert_eq!(summary.cutoff(), Some(cutoff_id));
}

/// An automatic trigger has no typed instructions, but still says nobody
/// asked for it.
#[test]
fn an_automatic_compaction_notes_it_was_not_requested() {
    let cutoff_id = MessageId::new();
    let Message::Compaction(summary) = summary_message(cutoff_id, Trigger::Auto, "the gist") else {
        panic!("a summary is a compaction message");
    };
    assert!(summary.content.contains("automatically"));
    assert!(summary.content.contains("the gist"));
    assert_eq!(summary.cutoff(), Some(cutoff_id));
}

/// A failure is recorded but must not become a boundary, and is
/// transcript-only.
#[test]
fn a_failure_is_not_a_summary() {
    let failure = failure_message("the provider timed out");
    assert!(!failure.visible_to_model());
    let Message::Compaction(failure) = failure else {
        panic!("a failure record is a compaction message");
    };
    assert_eq!(failure.outcome, CompactionOutcome::Failed);
    assert!(!failure.is_summary());
    assert_eq!(failure.cutoff(), None);
    assert!(failure.content.contains("the provider timed out"));
}

#[test]
fn no_protection_targets_the_last_message() {
    let history = vec![user("a"), user("b"), user("c")];
    assert_eq!(
        planned_cutoff(&history, None, None, None).map(|cutoff| cutoff.coverage_message_id),
        Some(history[2].message.message_id()),
    );
}

#[test]
fn an_empty_thread_has_nothing_to_compact() {
    assert_eq!(planned_cutoff(&[], None, None, None), None);
}

#[test]
fn a_protected_turn_with_a_predecessor_targets_the_predecessors_last_message() {
    let previous_turn = TurnId::from(MessageId::new());
    let current_turn = TurnId::from(MessageId::new());
    let history = vec![
        user_in(previous_turn, "a"),
        user_in(previous_turn, "b"),
        user_in(current_turn, "c"),
        user_in(current_turn, "d"),
    ];
    assert_eq!(
        planned_cutoff(&history, Some(current_turn), None, Some(u32::MAX))
            .map(|cutoff| cutoff.coverage_message_id),
        Some(history[1].message.message_id())
    );
}

#[test]
fn a_preferred_boundary_is_before_the_named_turn_even_when_it_is_not_last() {
    let first_turn = TurnId::from(MessageId::new());
    let preferred_turn = TurnId::from(MessageId::new());
    let later_turn = TurnId::from(MessageId::new());
    let history = vec![
        user_in(first_turn, "first"),
        user_in(preferred_turn, "preferred"),
        user_in(later_turn, "later"),
    ];

    assert_eq!(
        planned_cutoff(&history, Some(preferred_turn), None, Some(u32::MAX))
            .map(|cutoff| cutoff.coverage_message_id),
        Some(history[0].message.message_id())
    );
}

/// The thread's very first task is protected before its first generation.
#[test]
fn a_protected_opening_message_with_no_predecessor_has_nothing_to_compact() {
    let only_turn = TurnId::from(MessageId::new());
    let opening = user_in(only_turn, "a");
    let protected = opening.message.message_id();
    let history = vec![opening];
    assert_eq!(
        planned_cutoff(&history, Some(only_turn), Some(protected), Some(u32::MAX)),
        None
    );
}

#[test]
fn a_first_turn_with_a_completed_batch_falls_back_inside_the_turn() {
    let turn = TurnId::from(MessageId::new());
    let history = vec![
        user_in(turn, "task"),
        assistant_in(turn, "call", Some(1_000)),
        tool_in(turn, "call"),
    ];
    assert_eq!(
        planned_cutoff(&history, Some(turn), None, Some(800))
            .map(|cutoff| cutoff.coverage_message_id),
        Some(history[2].message.message_id())
    );
}

#[test]
fn a_previous_turn_is_preferred_when_current_growth_is_below_threshold() {
    let previous_turn = TurnId::from(MessageId::new());
    let current_turn = TurnId::from(MessageId::new());
    let history = vec![
        user_in(previous_turn, "old"),
        user_in(current_turn, "task"),
        assistant_in(current_turn, "call", Some(900)),
        tool_in(current_turn, "call"),
    ];
    assert_eq!(
        planned_cutoff(&history, Some(current_turn), None, Some(800))
            .map(|cutoff| cutoff.coverage_message_id),
        Some(history[0].message.message_id())
    );
}

#[test]
fn measured_current_turn_growth_can_force_the_intra_turn_fallback() {
    let previous_turn = TurnId::from(MessageId::new());
    let current_turn = TurnId::from(MessageId::new());
    let history = vec![
        user_in(previous_turn, "old"),
        user_in(current_turn, "task"),
        assistant_in(current_turn, "first", Some(100)),
        tool_in(current_turn, "first"),
        assistant_in(current_turn, "second", Some(650)),
        tool_in(current_turn, "second"),
        assistant_in(current_turn, "third", Some(1_200)),
        tool_in(current_turn, "third"),
    ];
    assert_eq!(
        planned_cutoff(&history, Some(current_turn), None, Some(1_000))
            .map(|cutoff| cutoff.coverage_message_id),
        Some(history[7].message.message_id())
    );
}

#[test]
fn prompt_growth_starts_over_at_the_latest_summary() {
    let turn = TurnId::from(MessageId::new());
    let first_assistant = assistant_in(turn, "first", Some(100));
    let first_tool = tool_in(turn, "first");
    let summary = entry_in(
        turn,
        summary_message(first_tool.message.message_id(), Trigger::Auto, "gist"),
    );
    let history = vec![
        user_in(turn, "task"),
        first_assistant,
        first_tool,
        summary,
        assistant_in(turn, "second", Some(1_200)),
        tool_in(turn, "second"),
    ];
    let (_, tail) = message_view::model_view_parts_indexed(&history);
    let visible: Vec<_> = tail.map(VisibleEntry::from).collect();

    assert_eq!(prompt_growth(&visible, turn), Some(0));
}

#[test]
fn missing_or_non_monotonic_usage_does_not_override_the_turn_boundary() {
    let previous_turn = TurnId::from(MessageId::new());
    let current_turn = TurnId::from(MessageId::new());
    for second_usage in [None, Some(50)] {
        let history = vec![
            user_in(previous_turn, "old"),
            user_in(current_turn, "task"),
            assistant_in(current_turn, "first", Some(100)),
            tool_in(current_turn, "first"),
            assistant_in(current_turn, "second", second_usage),
            tool_in(current_turn, "second"),
        ];
        assert_eq!(
            planned_cutoff(&history, Some(current_turn), None, Some(1))
                .map(|cutoff| cutoff.coverage_message_id),
            Some(history[0].message.message_id())
        );
    }
}

/// Nothing has happened since the last compaction — `cutoff` refuses rather
/// than summarizing the summary.
#[test]
fn a_target_at_or_before_the_existing_boundary_is_nothing_new() {
    let root = user("root");
    let root_id = root.message.message_id();
    let summary = entry(summary_message(
        root_id,
        Trigger::Manual { instructions: "" },
        "gist",
    ));
    let history = vec![root, summary];
    assert_eq!(planned_cutoff(&history, None, None, None), None);
}

/// A new task remains raw when the previous-turn boundary is already covered.
#[test]
fn a_summary_does_not_cause_a_fresh_task_to_compact_itself() {
    let previous_turn = TurnId::from(MessageId::new());
    let current_turn = TurnId::from(MessageId::new());
    let previous = user_in(previous_turn, "a");
    let previous_id = previous.message.message_id();
    let summary = entry_in(
        current_turn,
        summary_message(previous_id, Trigger::Auto, "gist"),
    );
    let opening = user_in(current_turn, "b, appended after the summary landed");
    let protected = opening.message.message_id();
    let history = vec![previous, summary, opening];
    assert_eq!(
        planned_cutoff(
            &history,
            Some(current_turn),
            Some(protected),
            Some(u32::MAX)
        ),
        None
    );
}

#[test]
fn a_long_turn_can_compact_again_after_new_work() {
    let previous_turn = TurnId::from(MessageId::new());
    let current_turn = TurnId::from(MessageId::new());
    let previous = user_in(previous_turn, "old");
    let opening = user_in(current_turn, "task");
    let first_assistant = assistant_in(current_turn, "first", Some(100));
    let first_tool = tool_in(current_turn, "first");
    let first_cutoff = first_tool.message.message_id();
    let summary = entry_in(
        current_turn,
        summary_message(first_cutoff, Trigger::Auto, "first gist"),
    );
    let second_assistant = assistant_in(current_turn, "second", Some(1_000));
    let second_tool = tool_in(current_turn, "second");
    let expected = second_tool.message.message_id();
    let history = vec![
        previous,
        opening,
        first_assistant,
        first_tool,
        summary,
        second_assistant,
        second_tool,
    ];
    assert_eq!(
        planned_cutoff(&history, Some(current_turn), None, Some(800))
            .map(|cutoff| cutoff.coverage_message_id),
        Some(expected)
    );
}

#[test]
fn a_reordered_summary_uses_separate_logical_extent_and_physical_coverage() {
    let previous_turn = TurnId::from(MessageId::new());
    let compacted_turn = TurnId::from(MessageId::new());
    let next_turn = TurnId::from(MessageId::new());
    let previous = user_in(previous_turn, "RAW_REPLACED_MARKER");
    let opening = user_in(compacted_turn, "task");
    let assistant = assistant_in(compacted_turn, "call", Some(1_000));
    let tool = tool_in(compacted_turn, "call");
    let first_summary = entry_in(
        compacted_turn,
        summary_message(
            previous.message.message_id(),
            Trigger::Auto,
            "EARLIER_SUMMARY_MARKER",
        ),
    );
    let first_summary_id = first_summary.message.message_id();
    let fresh_task = user_in(next_turn, "next task");
    let fresh_task_id = fresh_task.message.message_id();
    let history = vec![
        previous,
        opening,
        assistant,
        tool,
        first_summary,
        fresh_task,
    ];

    let cutoff = planned_cutoff(
        &history,
        Some(next_turn),
        Some(fresh_task_id),
        Some(u32::MAX),
    )
    .expect("the completed turn is compactable");
    assert_eq!(cutoff.model_view_len, 4);
    assert_eq!(cutoff.coverage_message_id, first_summary_id);

    let request = summary_request(
        "model".to_string(),
        None,
        None,
        message_view::model_view(&history).take(cutoff.model_view_len),
        "",
    );
    let transcript = request_text(&request);
    assert!(transcript.contains("EARLIER_SUMMARY_MARKER"));
    assert!(!transcript.contains("RAW_REPLACED_MARKER"));

    let replacement = entry_in(
        next_turn,
        summary_message(cutoff.coverage_message_id, Trigger::Auto, "replacement"),
    );
    let replacement_id = replacement.message.message_id();
    let mut committed = history;
    committed.push(replacement);
    let visible_ids: Vec<_> = message_view::model_view(&committed)
        .map(|entry| entry.message.message_id())
        .collect();
    assert_eq!(visible_ids, [replacement_id, fresh_task_id]);
}

#[test]
fn malformed_history_is_an_error_instead_of_no_new_content() {
    let turn = TurnId::from(MessageId::new());
    let mut assistant = assistant_in(turn, "first", Some(100));
    let Message::Assistant(message) = &mut assistant.message else {
        unreachable!()
    };
    message.tool_calls.push(ToolCall {
        id: "second".to_string(),
        name: "tool".to_string(),
        arguments: Some("{}".to_string()),
    });
    let assistant_id = assistant.message.message_id();
    let history = vec![user_in(turn, "task"), assistant, tool_in(turn, "first")];
    assert_eq!(
        cutoff(&history, Some(turn), None, Some(1)),
        Err(message_view::InvalidHistory::IncompleteToolBatch {
            assistant_message_id: assistant_id,
            missing_call_ids: vec!["second".to_string()],
        })
    );
}

/// Reproduces the manual `/compact` commit shape: `cutoff` is computed before
/// `command` exists, then both are appended together, landing `command`
/// between `cutoff`'s target and the summary. The recorded boundary must
/// still exclude `command`, which only holds if it's `command`'s own id, not
/// the pre-command target `compaction::cutoff` returned.
#[test]
fn manual_compaction_hides_its_own_command_line_from_the_model_view() {
    let history = vec![user("old")];
    let cutoff_id = planned_cutoff(&history, None, None, None)
        .expect("something to compact")
        .coverage_message_id;

    let command = entry(command_message("keep the plan"));
    let outcome = entry(summary_message(
        command.message.message_id(),
        Trigger::Manual {
            instructions: "keep the plan",
        },
        "the gist",
    ));

    let mut committed = history;
    committed.push(command);
    committed.push(outcome);

    let texts: Vec<_> = message_view::model_view(&committed)
        .filter_map(|entry| match &entry.message {
            Message::Compaction(compaction) => Some(compaction.content.clone()),
            Message::User(user) => user.first_text().map(str::to_string),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["[compacted at the user's request: keep the plan]\n\nthe gist".to_string()],
        "the command line must stay out of the model's view, same as {cutoff_id:?} \
         being superseded by the command's own id"
    );
}

/// A failed attempt isn't a summary, so it doesn't move the boundary
/// — the next check in the same turn retries the same target.
#[test]
fn a_failed_attempt_does_not_suppress_a_later_retry() {
    let previous_turn = TurnId::from(MessageId::new());
    let current_turn = TurnId::from(MessageId::new());
    let previous = user_in(previous_turn, "a");
    let previous_id = previous.message.message_id();
    let history = vec![
        previous,
        entry_in(current_turn, failure_message("the provider timed out")),
        user_in(current_turn, "b"),
    ];
    assert_eq!(
        planned_cutoff(&history, Some(current_turn), None, Some(u32::MAX))
            .map(|cutoff| cutoff.coverage_message_id),
        Some(previous_id)
    );
}
