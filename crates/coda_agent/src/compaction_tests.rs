use super::*;
use coda_core::llm::{AssistantMessage, RequestMessage, ToolCall, ToolCallOutcome, ToolMessage};

fn entry(message: Message) -> HistoryEntry {
    HistoryEntry {
        turn_id: TurnId::from(MessageId::new()),
        message,
    }
}

fn entry_in(turn: TurnId, message: Message) -> HistoryEntry {
    HistoryEntry {
        turn_id: turn,
        message,
    }
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

/// The request is one instruction over a record, not a conversation — so it
/// carries no tools, and nothing in it can be an orphan tool result.
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
    let Message::Custom(summary) = summary_message(
        cutoff_id,
        Trigger::Manual {
            instructions: "keep the architecture decisions",
        },
        "the gist",
    ) else {
        panic!("a summary is a custom message");
    };
    assert_eq!(summary.kind, COMPACTION_KIND);
    assert_eq!(summary.cutoff, Some(cutoff_id));
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
    let Message::Custom(summary) =
        summary_message(cutoff_id, Trigger::Manual { instructions: "" }, "the gist")
    else {
        panic!("a summary is a custom message");
    };
    assert_eq!(summary.content, "the gist");
    assert_eq!(summary.cutoff, Some(cutoff_id));
}

/// An automatic trigger has no typed instructions, but still says (to a human
/// reading the transcript) that nobody asked for it.
#[test]
fn an_automatic_compaction_notes_it_was_not_requested() {
    let cutoff_id = MessageId::new();
    let Message::Custom(summary) = summary_message(cutoff_id, Trigger::Auto, "the gist") else {
        panic!("a summary is a custom message");
    };
    assert!(summary.content.contains("automatically"));
    assert!(summary.content.contains("the gist"));
    assert_eq!(summary.cutoff, Some(cutoff_id));
}

/// A failure is recorded but must not become a boundary, or a compaction that
/// did not happen would hide the conversation anyway. It is transcript-only
/// (no role), so the model view never pays for it.
#[test]
fn a_failure_is_not_a_summary() {
    let Message::Custom(failure) = failure_message("the provider timed out") else {
        panic!("a failure record is a custom message");
    };
    assert_eq!(failure.kind, COMPACTION_FAILED_KIND);
    assert_ne!(failure.kind, COMPACTION_KIND);
    assert_eq!(failure.role, None);
    assert_eq!(failure.cutoff, None);
    assert!(failure.content.contains("the provider timed out"));
}

#[test]
fn no_protection_targets_the_last_message() {
    let history = vec![user("a"), user("b"), user("c")];
    assert_eq!(
        cutoff(&history, None),
        Some(history[2].message.message_id())
    );
}

#[test]
fn an_empty_thread_has_nothing_to_compact() {
    assert_eq!(cutoff(&[], None), None);
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
        cutoff(&history, Some(current_turn)),
        Some(history[1].message.message_id())
    );
}

/// A turn with no predecessor — the thread's very first turn — has nothing
/// before it a protecting compaction is allowed to touch.
#[test]
fn a_protected_turn_with_no_predecessor_has_nothing_to_compact() {
    let only_turn = TurnId::from(MessageId::new());
    let history = vec![user_in(only_turn, "a"), user_in(only_turn, "b")];
    assert_eq!(cutoff(&history, Some(only_turn)), None);
}

/// Nothing has happened since the last compaction, so compacting again would
/// only summarize the summary — `cutoff` refuses rather than doing that.
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
    assert_eq!(cutoff(&history, None), None);
}

/// Once a compaction has succeeded past the boundary a protected turn targets,
/// the same turn cannot compact again — `protect` pins the target to the same
/// message for as long as the turn is current, and that message now falls at
/// or before the existing boundary.
#[test]
fn a_turn_compacts_at_most_once() {
    let previous_turn = TurnId::from(MessageId::new());
    let current_turn = TurnId::from(MessageId::new());
    let previous = user_in(previous_turn, "a");
    let previous_id = previous.message.message_id();
    let summary = entry_in(
        current_turn,
        summary_message(previous_id, Trigger::Auto, "gist"),
    );
    let history = vec![
        previous,
        summary,
        user_in(current_turn, "b, appended after the summary landed"),
    ];
    assert_eq!(cutoff(&history, Some(current_turn)), None);
}

/// Reproduces the manual `/compact` commit shape end to end: `cutoff` is
/// computed before `command` exists, then `command` and the summary are
/// appended together in one commit, physically landing `command` between
/// `cutoff`'s target and the summary. The summary must still record a
/// boundary that excludes `command` — the model was never meant to see the
/// raw "/compact ..." line — which only holds if the recorded `cutoff` is
/// `command`'s own id, not the pre-command target `compaction::cutoff`
/// returned.
#[test]
fn manual_compaction_hides_its_own_command_line_from_the_model_view() {
    let history = vec![user("old")];
    let cutoff_id = cutoff(&history, None).expect("something to compact");

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
            Message::Custom(custom) => Some(custom.content.clone()),
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

/// A failed attempt does not move the boundary — it isn't `COMPACTION_KIND` —
/// so the next detection point in the same turn targets the same message
/// again rather than being permanently suppressed.
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
    assert_eq!(cutoff(&history, Some(current_turn)), Some(previous_id));
}
