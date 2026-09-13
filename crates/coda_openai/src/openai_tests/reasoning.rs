use super::super::*;

const GROK_FIXTURE: &str = include_str!("../../tests/fixtures/openrouter-grok-4.5.json");
const KIMI_FIXTURE: &str = include_str!("../../tests/fixtures/openrouter-kimi-k3.json");
const GLM_FIXTURE: &str = include_str!("../../tests/fixtures/openrouter-glm-5.2.json");

#[test]
fn reduced_completion_keeps_reasoning_content() {
    let mut completion = CompletionAccumulator::new();
    completion.reduce_reasoning("first ");
    completion.reduce_reasoning("second");
    completion.reduce_tool_chunk(ChatCompletionMessageToolCallChunk {
        index: 0,
        id: Some("call-1".into()),
        r#type: None,
        function: Some(FunctionCallStream {
            name: Some("shell".into()),
            arguments: Some("{}".into()),
        }),
    });

    let message = AssistantMessage::try_from(completion).unwrap();

    assert_eq!(message.reasoning_content.as_deref(), Some("first second"));
}

#[test]
fn reduced_completion_keeps_reasoning_without_tool_calls() {
    let mut completion = CompletionAccumulator::new();
    completion.reduce_reasoning("final reasoning");

    let message = AssistantMessage::try_from(completion).unwrap();

    assert_eq!(
        message.reasoning_content.as_deref(),
        Some("final reasoning")
    );
}

fn reduce_openrouter_fixture(fixture: &str) -> AssistantMessage {
    let responses: Vec<CompatibleStreamResponse> = serde_json::from_str(fixture).unwrap();
    let mut completion = CompletionAccumulator::new();
    let mut reasoning_chunks = 0;
    for response in responses {
        let events = ProviderKind::OpenRouter
            .reduce_response("openrouter", response, &mut completion)
            .unwrap();
        reasoning_chunks += events
            .iter()
            .filter(|event| matches!(event, LLMStreamEvent::ReasoningChunk(_)))
            .count();
    }
    assert_eq!(reasoning_chunks, 2);
    AssistantMessage::try_from(completion).unwrap()
}

#[test]
fn real_openrouter_fixtures_keep_ordered_reasoning_details() {
    let cases = [
        (GROK_FIXTURE, 3, "reasoning.summary", "The tool"),
        (KIMI_FIXTURE, 2, "reasoning.text", "Need tool"),
        (GLM_FIXTURE, 2, "reasoning.text", "The user"),
    ];

    for (fixture, expected_details, first_type, expected_reasoning) in cases {
        let message = reduce_openrouter_fixture(fixture);
        assert_eq!(
            message.reasoning_content.as_deref(),
            Some(expected_reasoning)
        );
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(
            message.tool_calls[0].arguments.as_deref(),
            Some("{\"city\":\"Singapore\"}")
        );
        let details = message
            .reasoning_continuation
            .as_ref()
            .and_then(|continuation| continuation.payload_for(OPENROUTER_REASONING_DETAILS_FORMAT))
            .and_then(serde_json::Value::as_array)
            .unwrap();
        assert_eq!(details.len(), expected_details);
        assert_eq!(details[0]["type"], serde_json::json!(first_type));
        assert!(message.usage.is_some());
    }
}

#[test]
fn openrouter_prefers_reasoning_then_alias_then_visible_details() {
    let responses: Vec<CompatibleStreamResponse> = serde_json::from_value(serde_json::json!([
        {
            "choices": [{"delta": {
                "reasoning": "primary",
                "reasoning_content": "alias",
                "reasoning_details": [{"type": "reasoning.text", "text": "fallback"}]
            }}]
        },
        {
            "choices": [{"delta": {
                "reasoning_content": " alias",
                "reasoning_details": [{"type": "reasoning.text", "text": " fallback"}]
            }}]
        },
        {
            "choices": [{"delta": {
                "reasoning_details": [{"type": "reasoning.summary", "summary": " detail"}]
            }}]
        }
    ]))
    .unwrap();
    let mut completion = CompletionAccumulator::new();
    for response in responses {
        ProviderKind::OpenRouter
            .reduce_response("openrouter", response, &mut completion)
            .unwrap();
    }

    let message = AssistantMessage::try_from(completion).unwrap();
    assert_eq!(
        message.reasoning_content.as_deref(),
        Some("primary alias detail")
    );
}
