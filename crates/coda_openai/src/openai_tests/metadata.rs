use super::super::*;
use super::fixtures::assistant;

#[test]
fn generation_metadata_roundtrips_in_history_but_never_enters_provider_messages() {
    let mut message = assistant();
    let old = serde_json::to_value(&message).unwrap();
    assert!(old.get("generation").is_none());
    assert!(
        serde_json::from_value::<AssistantMessage>(old)
            .unwrap()
            .generation
            .is_none()
    );
    message.content = "a historical answer".into();
    message.generation = Some(coda_core::llm::GenerationMetadata {
        provider_id: "historical-provider".into(),
        model_id: "historical-preview".into(),
        reasoning_effort: Some("high".into()),
        reported_model_id: Some("historical-upstream-version".into()),
    });
    let saved = serde_json::to_value(&message).unwrap();
    let mut legacy = saved.clone();
    legacy["generation"]
        .as_object_mut()
        .unwrap()
        .remove("reported_model_id");
    let legacy: AssistantMessage = serde_json::from_value(legacy).unwrap();
    assert!(legacy.generation.unwrap().reported_model_id.is_none());
    let restored: AssistantMessage = serde_json::from_value(saved).unwrap();
    assert_eq!(restored.generation, message.generation);
    for kind in [
        ProviderKind::Generic,
        ProviderKind::Deepseek,
        ProviderKind::OpenRouter,
    ] {
        let outgoing = kind
            .encode_request(
                ChatCompletionRequest {
                    model: "current-request".into(),
                    messages: vec![RequestMessage::Assistant(restored.clone())],
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        assert!(outgoing["messages"][0].get("generation").is_none());
        assert!(!outgoing.to_string().contains("historical-provider"));
        assert!(!outgoing.to_string().contains("historical-preview"));
        assert!(!outgoing.to_string().contains("historical-upstream-version"));
        assert_eq!(outgoing["messages"][0]["content"], "a historical answer");
    }
}

#[test]
fn reports_models_before_text_and_keeps_last_valid_value() {
    for kind in [
        ProviderKind::Generic,
        ProviderKind::Deepseek,
        ProviderKind::OpenRouter,
    ] {
        let responses: Vec<CompatibleStreamResponse> = serde_json::from_value(serde_json::json!([
            {"model": "first", "choices": [{"delta": {"reasoning_content": "think", "content": "answer"}}]},
            {"model": "first", "choices": []},
            {"model": "last", "choices": []},
            {},
            {"model": null},
            {"model": ""},
            {"model": " \n\t"},
            {"model": 42},
            {"model": ["not-a-model"]},
            {"model": {"id": "not-a-model"}},
            {"model": false}
        ])).unwrap();
        let mut completion = CompletionAccumulator::new();
        let events: Vec<_> = responses
            .into_iter()
            .flat_map(|response| {
                kind.reduce_response("configured-provider", response, &mut completion)
                    .unwrap()
            })
            .collect();
        assert!(matches!(events.as_slice(), [
            LLMStreamEvent::ModelReported(first),
            LLMStreamEvent::ReasoningChunk(reasoning),
            LLMStreamEvent::ContentChunk(content),
            LLMStreamEvent::ModelReported(last)
        ] if first == "first" && reasoning == "think" && content == "answer" && last == "last"));
        assert_eq!(completion.reported_model_id.as_deref(), Some("last"));
        let message = AssistantMessage::try_from(completion).unwrap();
        assert!(message.generation.is_none());
        assert!(message.usage.is_none());
    }
}

#[test]
fn model_only_response_does_not_become_an_assistant_message() {
    let response =
        serde_json::from_value(serde_json::json!({"model": " Vendor/Version "})).unwrap();
    let mut completion = CompletionAccumulator::new();
    let events = ProviderKind::Generic
        .reduce_response("provider", response, &mut completion)
        .unwrap();
    assert!(
        matches!(events.as_slice(), [LLMStreamEvent::ModelReported(model)] if model == " Vendor/Version ")
    );
    assert!(AssistantMessage::try_from(completion).is_err());
}

#[test]
fn error_envelope_does_not_update_reported_model() {
    let response = serde_json::from_value(serde_json::json!({
        "model": "failed-model",
        "error": {"code": 429, "message": "rate limited"}
    }))
    .unwrap();
    let mut completion = CompletionAccumulator::new();
    completion.reported_model_id = Some("previous-report".into());
    assert!(
        ProviderKind::OpenRouter
            .reduce_response("provider", response, &mut completion)
            .is_err()
    );
    assert_eq!(
        completion.reported_model_id.as_deref(),
        Some("previous-report")
    );
}
