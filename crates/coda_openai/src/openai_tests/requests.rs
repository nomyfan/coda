use super::super::*;
use super::fixtures::assistant;

#[test]
fn user_text_message_uses_text_content_form() {
    let message: ChatCompletionRequestMessage =
        RequestMessage::User(coda_core::llm::UserMessage::text(MessageId::new(), "hello"))
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
        RequestMessage::User(coda_core::llm::UserMessage::with_images(
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
        RequestMessage::Assistant(AssistantMessage {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "shell".into(),
                arguments: Some("{}".into()),
            }],
            reasoning_content: Some("need a tool".into()),
            ..assistant()
        }),
        RequestMessage::Assistant(AssistantMessage {
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
