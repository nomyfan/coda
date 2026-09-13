use super::super::*;

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
