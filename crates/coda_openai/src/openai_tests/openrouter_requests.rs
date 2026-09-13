use super::super::*;
use super::fixtures::assistant;

#[test]
fn openrouter_replays_details_and_maps_off_effort_to_none() {
    let continuation = ReasoningContinuation::try_new(
        OPENROUTER_REASONING_DETAILS_FORMAT,
        serde_json::json!([
            {"type": "reasoning.summary", "summary": "first", "index": 0},
            {"type": "reasoning.encrypted", "data": "opaque", "index": 1}
        ]),
    )
    .unwrap();
    let request = ChatCompletionRequest {
        model: "x-ai/grok-4.5".into(),
        messages: vec![RequestMessage::Assistant(AssistantMessage {
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "lookup_weather".into(),
                arguments: Some("{}".into()),
            }],
            reasoning_content: Some("visible".into()),
            reasoning_continuation: Some(continuation),
            ..assistant()
        })],
        reasoning_effort: Some("off".into()),
        ..Default::default()
    };

    let body = ProviderKind::OpenRouter
        .encode_request(request, true)
        .unwrap();

    assert_eq!(body["reasoning"]["effort"], serde_json::json!("none"));
    assert_eq!(
        body["messages"][0]["reasoning_details"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(body["messages"][0].get("reasoning").is_none());
    assert!(body.get("max_completion_tokens").is_none());
    assert_eq!(
        body["stream_options"]["include_usage"],
        serde_json::json!(true)
    );
}

#[test]
fn openrouter_classifies_malformed_continuation_as_invalid_request() {
    let continuation = ReasoningContinuation::try_new(
        OPENROUTER_REASONING_DETAILS_FORMAT,
        serde_json::json!({"unexpected": "object"}),
    )
    .unwrap();
    let request = ChatCompletionRequest {
        model: "x-ai/grok-4.5".into(),
        messages: vec![RequestMessage::Assistant(AssistantMessage {
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "lookup_weather".into(),
                arguments: Some("{}".into()),
            }],
            reasoning_continuation: Some(continuation),
            ..assistant()
        })],
        ..Default::default()
    };

    let error = ProviderKind::OpenRouter
        .encode_request(request, false)
        .unwrap_err();

    assert!(matches!(
        error,
        StreamError::InvalidRequest(ref message)
            if message == "OpenRouter reasoning continuation payload must be an array"
    ));
}

#[test]
fn openrouter_replays_plain_reasoning_only_for_tool_turns() {
    let request = ChatCompletionRequest {
        model: "moonshotai/kimi-k3".into(),
        messages: vec![
            RequestMessage::Assistant(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "lookup_weather".into(),
                    arguments: Some("{}".into()),
                }],
                reasoning_content: Some("tool reasoning".into()),
                ..assistant()
            }),
            RequestMessage::Assistant(AssistantMessage {
                content: "done".into(),
                reasoning_content: Some("final reasoning".into()),
                ..assistant()
            }),
        ],
        reasoning_effort: Some("high".into()),
        max_completion_tokens: Some(4096),
        ..Default::default()
    };

    let body = ProviderKind::OpenRouter
        .encode_request(request, false)
        .unwrap();

    assert_eq!(body["messages"][0]["reasoning"], "tool reasoning");
    assert!(body["messages"][1].get("reasoning").is_none());
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["max_completion_tokens"], 4096);
}

#[test]
fn openrouter_keeps_image_input_and_tool_continuation_in_one_request() {
    let continuation = ReasoningContinuation::try_new(
        OPENROUTER_REASONING_DETAILS_FORMAT,
        serde_json::json!([{
            "type": "reasoning.text",
            "text": "inspect image",
            "format": "unknown",
            "index": 0
        }]),
    )
    .unwrap();
    let request = ChatCompletionRequest {
        model: "moonshotai/kimi-k3".into(),
        messages: vec![
            RequestMessage::User(coda_core::llm::UserMessage::with_images(
                MessageId::new(),
                "inspect",
                &["data:image/png;base64,abc123".to_string()],
            )),
            RequestMessage::Assistant(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "lookup_weather".into(),
                    arguments: Some("{}".into()),
                }],
                reasoning_content: Some("inspect image".into()),
                reasoning_continuation: Some(continuation),
                ..assistant()
            }),
        ],
        reasoning_effort: Some("max".into()),
        ..Default::default()
    };

    let body = ProviderKind::OpenRouter
        .encode_request(request, false)
        .unwrap();

    assert_eq!(
        body["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,abc123"
    );
    assert_eq!(
        body["messages"][1]["reasoning_details"][0]["text"],
        "inspect image"
    );
}
