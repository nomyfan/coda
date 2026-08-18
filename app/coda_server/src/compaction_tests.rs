use super::*;
use coda_core::llm::{
    AssistantMessage, RequestMessage, ToolCall, ToolCallOutcome, ToolMessage, TurnId,
};

fn entry(message: Message) -> HistoryEntry {
    HistoryEntry {
        turn_id: TurnId::from(MessageId::new()),
        message,
    }
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

    let Message::Custom(summary) = summary_message("keep the architecture decisions", "the gist")
    else {
        panic!("a summary is a custom message");
    };
    assert_eq!(summary.kind, COMPACTION_KIND);
    assert!(summary.content.contains("keep the architecture decisions"));
    assert!(summary.content.contains("the gist"));
}

#[test]
fn a_bare_compact_adds_no_instruction_framing() {
    let Message::User(command) = command_message("") else {
        panic!("the command is a user message");
    };
    assert_eq!(command.first_text(), Some("/compact"));

    let Message::Custom(summary) = summary_message("", "the gist") else {
        panic!("a summary is a custom message");
    };
    assert_eq!(summary.content, "the gist");
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
    assert!(failure.content.contains("the provider timed out"));
}
