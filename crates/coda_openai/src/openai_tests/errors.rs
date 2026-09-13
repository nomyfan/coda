use super::super::*;

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
