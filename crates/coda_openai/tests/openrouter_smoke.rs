use coda_core::llm::{
    ChatCompletionRequest, LLMProvider, LLMProviderConfig, LLMStreamEvent, Message, StreamError,
    SystemMessage, ToolCallOutcome, ToolDefinition, ToolMessage, ToolOutput, UserMessage,
};
use coda_openai::{OpenAICompatible, ProviderKind};
use futures::StreamExt as _;

fn weather_tool() -> ToolDefinition {
    ToolDefinition {
        name: "lookup_weather".into(),
        description: "Return the current weather for a city.".into(),
        parameter_schema: serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
            "additionalProperties": false
        }),
    }
}

async fn completion(
    provider: &OpenAICompatible,
    request: ChatCompletionRequest,
) -> Result<coda_core::llm::AssistantMessage, StreamError> {
    let mut stream = std::pin::pin!(provider.stream(request));
    while let Some(event) = stream.next().await {
        match event? {
            LLMStreamEvent::Completed(message) => return Ok(*message),
            LLMStreamEvent::ContentChunk(_) | LLMStreamEvent::ReasoningChunk(_) => {}
        }
    }
    Err(StreamError::InvalidResponse(
        "OpenRouter stream ended without a completion".into(),
    ))
}

async fn completion_with_retry(
    provider: &OpenAICompatible,
    request: ChatCompletionRequest,
) -> coda_core::llm::AssistantMessage {
    for attempt in 1..=3 {
        match completion(provider, request.clone()).await {
            Ok(message) => return message,
            Err(StreamError::Provider(error)) if error.status_code == Some(429) && attempt < 3 => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(error) => panic!("OpenRouter stream failed: {error}"),
        }
    }
    unreachable!("retry loop returns on its final attempt")
}

#[tokio::test]
#[ignore = "requires OPENROUTER_API_KEY and makes six paid requests"]
async fn three_models_replay_reasoning_continuations_after_tool_calls() {
    let api_key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
    let provider = OpenAICompatible::new(
        LLMProviderConfig {
            api_key,
            base_url: "https://openrouter.ai/api/v1".into(),
            include_usage: true,
        },
        ProviderKind::OpenRouter,
        "openrouter",
    );
    let cases = [
        ("x-ai/grok-4.5", "low"),
        ("moonshotai/kimi-k3", "low"),
        ("z-ai/glm-5.2", "high"),
    ];

    for (model, effort) in cases {
        eprintln!("{model}: requesting tool call");
        let system = Message::System(SystemMessage(
            "You execute tasks with the provided tools. When the user asks for a tool call, you must call it and must not answer from memory."
                .into(),
        ));
        let user = Message::User(UserMessage::text(
            "Your only valid action is to call lookup_weather exactly once with city Singapore. Do not emit a direct answer.",
        ));
        let assistant = completion_with_retry(
            &provider,
            ChatCompletionRequest {
                model: model.into(),
                messages: vec![system.clone(), user.clone()],
                tools: vec![weather_tool()],
                max_completion_tokens: Some(2048),
                reasoning_effort: Some(effort.into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            assistant.tool_calls.len(),
            1,
            "{model} did not call the tool"
        );
        assert!(
            assistant.reasoning_continuation.is_some(),
            "{model} omitted reasoning_details"
        );
        eprintln!("{model}: tool call and reasoning continuation accepted");
        let tool_call = &assistant.tool_calls[0];
        let tool = Message::Tool(ToolMessage::new(
            tool_call.id.clone(),
            tool_call.name.clone(),
            ToolOutput::Ok(r#"{"temperature_c":30,"condition":"humid"}"#.into()),
            ToolCallOutcome::Auto,
            None,
        ));
        let final_message = completion_with_retry(
            &provider,
            ChatCompletionRequest {
                model: model.into(),
                messages: vec![system, user, Message::Assistant(assistant), tool],
                tools: vec![weather_tool()],
                max_completion_tokens: Some(2048),
                reasoning_effort: Some(effort.into()),
                ..Default::default()
            },
        )
        .await;
        assert!(
            final_message.tool_calls.is_empty(),
            "{model} called the tool again"
        );
        assert!(
            !final_message.content.is_empty() || final_message.reasoning_content.is_some(),
            "{model} returned no final output"
        );
        eprintln!("{model}: continuation replay accepted");
    }
}
