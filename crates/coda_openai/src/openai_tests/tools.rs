use super::super::*;

#[test]
fn accumulator_reassembles_interleaved_parallel_tool_calls() {
    let chunks: Vec<ChatCompletionMessageToolCallChunk> = serde_json::from_value(
        serde_json::json!([
            {"index": 0, "id": "call-0", "type": "function", "function": {"name": "first", "arguments": "{"}},
            {"index": 1, "id": "call-1", "type": "function", "function": {"name": "second", "arguments": "{"}},
            {"index": 0, "function": {"arguments": "}"}},
            {"index": 1, "function": {"arguments": "}"}}
        ]),
    )
    .unwrap();
    let mut completion = CompletionAccumulator::new();
    for chunk in chunks {
        completion.reduce_tool_chunk(chunk);
    }

    let message = AssistantMessage::try_from(completion).unwrap();
    assert_eq!(message.tool_calls.len(), 2);
    assert_eq!(message.tool_calls[0].arguments.as_deref(), Some("{}"));
    assert_eq!(message.tool_calls[1].arguments.as_deref(), Some("{}"));
}

#[test]
fn accumulator_rejects_empty_stream() {
    let error = AssistantMessage::try_from(CompletionAccumulator::new()).unwrap_err();
    assert_eq!(
        error,
        "stream completed without content, reasoning, or tool calls"
    );
}
