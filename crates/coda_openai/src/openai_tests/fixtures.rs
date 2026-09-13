use super::super::*;

pub(super) fn assistant() -> AssistantMessage {
    let now = jiff::Timestamp::now();
    AssistantMessage {
        generation: None,
        message_id: MessageId::new(),
        content: String::new(),
        tool_calls: vec![],
        usage: None,
        reasoning_content: None,
        reasoning_continuation: None,
        reasoning_ended_at: None,
        aborted: false,
        started_at: now,
        ended_at: now,
    }
}
