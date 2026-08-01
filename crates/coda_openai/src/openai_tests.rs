use super::*;

const GROK_FIXTURE: &str = include_str!("../tests/fixtures/openrouter-grok-4.5.json");
const KIMI_FIXTURE: &str = include_str!("../tests/fixtures/openrouter-kimi-k3.json");
const GLM_FIXTURE: &str = include_str!("../tests/fixtures/openrouter-glm-5.2.json");

/// Base assistant message for tests; callers override the fields they care
/// about with struct-update syntax (`..assistant()`).
fn assistant() -> AssistantMessage {
    let now = jiff::Timestamp::now();
    AssistantMessage {
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

#[test]
fn user_text_message_uses_text_content_form() {
    let message: ChatCompletionRequestMessage =
        Message::User(coda_core::llm::UserMessage::text(MessageId::new(), "hello"))
            .into_openai_type();

    let ChatCompletionRequestMessage::User(user) = message else {
        panic!("expected user message");
    };
    assert!(matches!(
        user.content,
        ChatCompletionRequestUserMessageContent::Text(text) if text == "hello"
    ));
}

#[test]
fn user_image_message_uses_array_content_form() {
    let image_url = "data:image/png;base64,abc123".to_string();
    let message: ChatCompletionRequestMessage =
        Message::User(coda_core::llm::UserMessage::with_images(
            MessageId::new(),
            "look",
            std::slice::from_ref(&image_url),
        ))
        .into_openai_type();

    let ChatCompletionRequestMessage::User(user) = message else {
        panic!("expected user message");
    };
    let ChatCompletionRequestUserMessageContent::Array(parts) = user.content else {
        panic!("expected array content");
    };

    assert_eq!(parts.len(), 2);
    assert!(matches!(
        &parts[0],
        ChatCompletionRequestUserMessageContentPart::Text(text) if text.text == "look"
    ));
    assert!(matches!(
        &parts[1],
        ChatCompletionRequestUserMessageContentPart::ImageUrl(image)
            if image.image_url.url == image_url && image.image_url.detail.is_none()
    ));
}

#[test]
fn injects_reasoning_only_for_assistant_tool_calls() {
    let messages = vec![
        Message::Assistant(AssistantMessage {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "shell".into(),
                arguments: Some("{}".into()),
            }],
            reasoning_content: Some("need a tool".into()),
            ..assistant()
        }),
        Message::Assistant(AssistantMessage {
            content: "done".into(),
            reasoning_content: Some("final reasoning".into()),
            ..assistant()
        }),
    ];
    let mut body = serde_json::json!({
        "messages": [
            {"role": "assistant", "tool_calls": [{}]},
            {"role": "assistant", "content": "done"}
        ]
    });

    inject_deepseek_reasoning(&mut body, &messages);

    assert_eq!(
        body["messages"][0]["reasoning_content"],
        serde_json::json!("need a tool")
    );
    assert!(body["messages"][1].get("reasoning_content").is_none());
}

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

#[test]
fn stream_usage_option_serializes_in_request_body() {
    let request = CreateChatCompletionRequestArgs::default()
        .model("test-model")
        .messages(Vec::<ChatCompletionRequestMessage>::new())
        .stream(true)
        .stream_options(ChatCompletionStreamOptions {
            include_usage: Some(true),
            include_obfuscation: None,
        })
        .build()
        .unwrap();

    let body = serde_json::to_value(request).unwrap();

    assert_eq!(
        body["stream_options"]["include_usage"],
        serde_json::json!(true)
    );
}

#[test]
fn deepseek_usage_keeps_standard_and_cache_details() {
    let usage: ProviderCompletionUsage = serde_json::from_value(serde_json::json!({
        "prompt_tokens": 120,
        "completion_tokens": 30,
        "total_tokens": 150,
        "prompt_tokens_details": {
            "audio_tokens": 4,
            "cached_tokens": 80
        },
        "completion_tokens_details": {
            "accepted_prediction_tokens": 2,
            "audio_tokens": 3,
            "reasoning_tokens": 20,
            "rejected_prediction_tokens": 1
        },
        "prompt_cache_hit_tokens": 75,
        "prompt_cache_miss_tokens": 45
    }))
    .unwrap();

    let usage = usage.into_completion_usage(ProviderKind::Deepseek);

    assert_eq!(usage.total_tokens, 150);
    assert_eq!(
        usage.prompt_tokens_details,
        Some(PromptTokensDetails {
            audio_tokens: Some(4),
            cached_tokens: Some(80),
            cache_hit_tokens: Some(75),
            cache_miss_tokens: Some(45),
        })
    );
    assert_eq!(
        usage.completion_tokens_details,
        Some(CompletionTokensDetails {
            accepted_prediction_tokens: Some(2),
            audio_tokens: Some(3),
            reasoning_tokens: Some(20),
            rejected_prediction_tokens: Some(1),
        })
    );
}

#[test]
fn generic_usage_uses_standard_details() {
    let usage: ProviderCompletionUsage = serde_json::from_value(serde_json::json!({
        "prompt_tokens": 120,
        "completion_tokens": 30,
        "total_tokens": 150,
        "prompt_tokens_details": {
            "cached_tokens": 80
        },
        "prompt_cache_hit_tokens": 75,
        "prompt_cache_miss_tokens": 45
    }))
    .unwrap();

    let usage = usage.into_completion_usage(ProviderKind::Generic);

    assert_eq!(
        usage.prompt_tokens_details,
        Some(PromptTokensDetails {
            cached_tokens: Some(80),
            ..Default::default()
        })
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
        messages: vec![Message::Assistant(AssistantMessage {
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
        messages: vec![Message::Assistant(AssistantMessage {
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
            Message::Assistant(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "lookup_weather".into(),
                    arguments: Some("{}".into()),
                }],
                reasoning_content: Some("tool reasoning".into()),
                ..assistant()
            }),
            Message::Assistant(AssistantMessage {
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
            Message::User(coda_core::llm::UserMessage::with_images(
                MessageId::new(),
                "inspect",
                &["data:image/png;base64,abc123".to_string()],
            )),
            Message::Assistant(AssistantMessage {
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

#[test]
fn openrouter_rejects_stream_error_envelope() {
    let response: CompatibleStreamResponse = serde_json::from_value(serde_json::json!({
        "error": {
            "code": 429,
            "message": "rate limited",
            "metadata": {"error_type": "rate_limit_exceeded"}
        }
    }))
    .unwrap();
    let mut completion = CompletionAccumulator::new();

    let error = match ProviderKind::OpenRouter.reduce_response(
        "openrouter-primary",
        response,
        &mut completion,
    ) {
        Ok(_) => panic!("expected provider error"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StreamError::Provider(ProviderError {
            ref provider_id,
            status_code: Some(429),
            error_type: Some(ref error_type),
            ref message,
        }) if provider_id == "openrouter-primary"
            && error_type == "rate_limit_exceeded"
            && message == "rate limited"
    ));
}

#[test]
fn openrouter_recovers_structured_non_success_error_from_raw_body() {
    let deserialize_error = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
    let error = map_request_error(
        ProviderKind::OpenRouter,
        "openrouter-backup",
        OpenAIError::JSONDeserialize(
            deserialize_error,
            serde_json::json!({
                "error": {
                    "code": 429,
                    "message": "Provider returned error",
                    "metadata": {"error_type": "rate_limit_exceeded"}
                }
            })
            .to_string(),
        ),
    );

    assert!(matches!(
        error,
        StreamError::Provider(ProviderError {
            ref provider_id,
            status_code: Some(429),
            error_type: Some(ref error_type),
            ref message,
        }) if provider_id == "openrouter-backup"
            && error_type == "rate_limit_exceeded"
            && message == "Provider returned error"
    ));
}

#[test]
fn deepseek_http_api_error_is_a_provider_error() {
    let error = map_request_error(
        ProviderKind::Deepseek,
        "deepseek-primary",
        OpenAIError::ApiError(async_openai::error::ApiErrorResponse {
            status_code: "422".parse().unwrap(),
            api_error: async_openai::error::ApiError {
                message: "invalid request".into(),
                r#type: Some("invalid_request_error".into()),
                param: None,
                code: Some("invalid_request_error".into()),
            },
        }),
    );

    assert!(matches!(
        error,
        StreamError::Provider(ProviderError {
            ref provider_id,
            status_code: Some(422),
            error_type: Some(ref error_type),
            ref message,
        }) if provider_id == "deepseek-primary"
            && error_type == "invalid_request_error"
            && message == "invalid request"
    ));
}

#[test]
fn compatible_non_success_body_is_still_a_provider_error() {
    let deserialize_error = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
    let error = map_request_error(
        ProviderKind::Deepseek,
        "deepseek-primary",
        OpenAIError::JSONDeserialize(
            deserialize_error,
            serde_json::json!({
                "error": {
                    "message": "provider rejected request",
                    "type": "invalid_request_error",
                    "code": "invalid_parameter"
                }
            })
            .to_string(),
        ),
    );

    assert!(matches!(
        error,
        StreamError::Provider(ProviderError {
            ref provider_id,
            status_code: None,
            error_type: Some(ref error_type),
            ref message,
        }) if provider_id == "deepseek-primary"
            && error_type == "invalid_request_error"
            && message == "provider rejected request"
    ));
}

#[test]
fn malformed_sse_payload_is_an_invalid_response() {
    let deserialize_error = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
    let error = map_stream_error(
        "deepseek-primary",
        OpenAIError::JSONDeserialize(deserialize_error, "not-json".into()),
    );

    assert!(matches!(
        error,
        StreamError::InvalidResponse(ref message)
            if message.contains("failed to decode provider SSE event")
                && message.contains("not-json")
    ));
}

#[test]
fn sse_transport_failure_is_a_transport_error() {
    let error = map_stream_error(
        "deepseek-primary",
        OpenAIError::StreamError(Box::new(async_openai::error::StreamError::EventStream(
            "connection reset".into(),
        ))),
    );

    assert!(matches!(
        error,
        StreamError::TransportError(ref message) if message.contains("connection reset")
    ));
}

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
