use super::*;

/// Responses profiles must retain their explicit route and model list rather
/// than gaining provider discovery or an implicit provider-specific model.
#[test]
fn profile_round_trips_explicit_models() {
    let profile: ResponsesProvider = serde_json::from_value(serde_json::json!({
        "base_url": "https://example.test/v1",
        "api_key": "secret",
        "models": [{"id": "example-model"}]
    }))
    .expect("profile");
    let encoded = serde_json::to_value(&profile).expect("serialize profile");
    assert_eq!(encoded["base_url"], "https://example.test/v1");
    assert_eq!(encoded["models"][0]["id"], "example-model");
    assert!(encoded.get("protocol_preset").is_none());
}

/// The public profile must publish only its explicitly configured model with
/// the Function/text metadata surface; reasoning remains response content, not
/// a configurable reasoning-control capability.
#[test]
fn profile_publishes_explicit_responses_model() {
    let provider = ResponsesProvider {
        base_url: "https://example.test/v1".to_owned(),
        api_key: String::new(),
        models: vec![ResponsesModel {
            id: tau_proto::ModelName::new("example-model"),
            display_name: None,
            context_window: 42,
            tags: Vec::new(),
            supports_parallel_tool_calls: true,
            est_uncached_input_cost_1m_usd: None,
            est_cached_input_cost_1m_usd: None,
            est_output_cost_1m_usd: None,
        }],
        tags: Vec::new(),
        max_output_tokens: 0,
    };
    let models = models_for_provider(&tau_proto::ProviderName::new("responses"), &provider);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id.to_string(), "responses/example-model");
    assert_eq!(
        models[0].supported_tool_types,
        vec![tau_proto::ToolType::Function]
    );
    assert!(!models[0].supports_compaction);
}

/// Public Responses plain reasoning progress must use the established full
/// reasoning delta so the existing `show-thinking` policy controls visibility.
#[test]
fn plain_reasoning_progress_emits_append_only_full_thinking() {
    let mut sampler = sampling::ResponsesResponseSampler::new();
    sampler.latest_items = vec![tau_provider_responses::AttemptOutputItem {
        output_index: 2,
        item: tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text: "think".to_owned(),
        }),
    }];
    assert_eq!(
        sampler.deltas(),
        vec![tau_proto::ProviderResponseTextDelta::ReasoningText {
            output_index: 2,
            kind: tau_proto::ReasoningTextKind::Full,
            text: "think".to_owned(),
        }]
    );

    sampler.latest_items = vec![tau_provider_responses::AttemptOutputItem {
        output_index: 2,
        item: tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text: "think again".to_owned(),
        }),
    }];
    assert_eq!(
        sampler.deltas(),
        vec![tau_proto::ProviderResponseTextDelta::ReasoningText {
            output_index: 2,
            kind: tau_proto::ReasoningTextKind::Full,
            text: " again".to_owned(),
        }]
    );

    sampler.latest_items = vec![tau_provider_responses::AttemptOutputItem {
        output_index: 2,
        item: tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text: "think revised".to_owned(),
        }),
    }];
    assert!(
        sampler.deltas().is_empty(),
        "sampler must never publish a non-append replacement if an upstream parser regresses"
    );
}
