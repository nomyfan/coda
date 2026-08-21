use super::tests::entry;
use super::*;
use coda_core::llm::{
    AssistantMessage, MessageId, ToolCall, ToolCallOutcome, ToolMessage, ToolOutput, UserMessage,
};

fn user() -> HistoryEntry {
    entry(Message::User(UserMessage::text(MessageId::new(), "task")))
}

fn assistant(calls: &[&str]) -> HistoryEntry {
    entry(Message::Assistant(AssistantMessage {
        message_id: MessageId::new(),
        content: String::new(),
        tool_calls: calls
            .iter()
            .map(|id| ToolCall {
                id: (*id).to_string(),
                name: "tool".to_string(),
                arguments: Some("{}".to_string()),
            })
            .collect(),
        usage: None,
        reasoning_content: None,
        reasoning_continuation: None,
        reasoning_ended_at: None,
        aborted: false,
        started_at: jiff::Timestamp::default(),
        ended_at: jiff::Timestamp::default(),
    }))
}

fn tool(id: &str) -> HistoryEntry {
    entry(Message::Tool(ToolMessage::new(
        id,
        "tool",
        ToolOutput::Ok("done".to_string()),
        ToolCallOutcome::Auto,
        None,
    )))
}

#[test]
fn a_complete_parallel_batch_is_valid_in_any_result_order() {
    let history = vec![user(), assistant(&["a", "b"]), tool("b"), tool("a")];
    assert_eq!(validate_model_view(&history), Ok(()));
}

#[test]
fn a_missing_tool_result_is_invalid() {
    let assistant = assistant(&["a", "b"]);
    let assistant_id = assistant.message.message_id();
    let history = vec![user(), assistant, tool("a")];
    assert_eq!(
        validate_model_view(&history),
        Err(InvalidHistory::IncompleteToolBatch {
            assistant_message_id: assistant_id,
            missing_call_ids: vec!["b".to_string()],
        })
    );
}

#[test]
fn an_orphan_tool_result_is_invalid() {
    let orphan = tool("a");
    let message_id = orphan.message.message_id();
    assert_eq!(
        validate_model_view(&[user(), orphan]),
        Err(InvalidHistory::OrphanToolResult {
            message_id,
            call_id: "a".to_string(),
        })
    );
}

#[test]
fn a_duplicate_tool_call_id_is_invalid() {
    let assistant = assistant(&["a", "a"]);
    let message_id = assistant.message.message_id();
    assert_eq!(
        validate_model_view(&[user(), assistant]),
        Err(InvalidHistory::DuplicateToolCall {
            message_id,
            call_id: "a".to_string(),
        })
    );
}

#[test]
fn a_duplicate_tool_result_is_invalid() {
    let duplicate = tool("a");
    let message_id = duplicate.message.message_id();
    let history = vec![user(), assistant(&["a"]), tool("a"), duplicate];
    assert_eq!(
        validate_model_view(&history),
        Err(InvalidHistory::DuplicateToolResult {
            message_id,
            call_id: "a".to_string(),
        })
    );
}

#[test]
fn a_result_from_another_batch_is_invalid() {
    let wrong = tool("a");
    let message_id = wrong.message.message_id();
    let history = vec![
        user(),
        assistant(&["a"]),
        tool("a"),
        assistant(&["b"]),
        wrong,
    ];
    assert_eq!(
        validate_model_view(&history),
        Err(InvalidHistory::OrphanToolResult {
            message_id,
            call_id: "a".to_string(),
        })
    );
}
