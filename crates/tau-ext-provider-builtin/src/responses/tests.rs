use super::*;

/// Public Responses accepts only its legacy retention cache contract, keeping
/// explicit boundaries out until they can preserve `instructions` semantics.
#[test]
fn profile_accepts_legacy_cache_retention_and_rejects_explicit_options() {
    let profile: ResponsesProvider = serde_json::from_value(serde_json::json!({
        "compat": {
            "openai_prompt_cache": {
                "key": "agent",
                "retention": "24h"
            }
        }
    }))
    .expect("legacy cache profile");
    assert_eq!(
        profile.compat.openai_prompt_cache,
        Some(OpenAiPromptCache {
            key: crate::OpenAiPromptCacheKey::Agent,
            retention: crate::OpenAiPromptCacheRetention::Hours24,
        })
    );

    let unsupported = serde_json::from_value::<ResponsesProvider>(serde_json::json!({
        "compat": {
            "openai_prompt_cache": {
                "key": "agent",
                "options": {"mode": "explicit"}
            }
        }
    }));
    assert!(unsupported.is_err());
}

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
    assert!(encoded["models"][0].get("efforts").is_none());
    assert!(encoded.get("protocol_preset").is_none());
    assert_eq!(encoded["transport"], "sse");
}

/// Profiles written before transport selection must remain SSE, while explicit
/// WebSocket selection must round-trip using the public configuration spelling.
#[test]
fn profile_transport_defaults_and_round_trips() {
    let legacy: ResponsesProvider =
        serde_json::from_value(serde_json::json!({})).expect("legacy profile");
    assert_eq!(legacy.transport, tau_provider_responses::Transport::Sse);
    let websocket: ResponsesProvider = serde_json::from_value(serde_json::json!({
        "transport": "websocket"
    }))
    .expect("WebSocket profile");
    assert_eq!(
        websocket.transport,
        tau_provider_responses::Transport::Websocket
    );
    assert_eq!(
        serde_json::to_value(websocket).expect("serialize")["transport"],
        "websocket"
    );
}

/// Final provider metadata must report the selected public Responses transport
/// rather than retaining the historical unconditional HTTP/SSE label.
#[test]
fn backend_metadata_matches_profile_transport() {
    let mut provider = ResponsesProvider::default();
    assert_eq!(
        backend_transport(&provider),
        tau_proto::ProviderBackendTransport::HttpSse
    );
    provider.transport = tau_provider_responses::Transport::Websocket;
    assert_eq!(
        backend_transport(&provider),
        tau_proto::ProviderBackendTransport::Websocket
    );
}

/// An omitted model effort override must retain the complete canonical public
/// Responses capability set, so existing profiles gain the documented default.
#[test]
fn profile_publishes_default_responses_efforts() {
    let provider = ResponsesProvider {
        base_url: "https://example.test/v1".to_owned(),
        api_key: String::new(),
        models: vec![ResponsesModel {
            id: tau_proto::ModelName::new("example-model"),
            efforts: None,
            compat: None,
            display_name: None,
            context_window: 42,
            tags: Vec::new(),
            supports_parallel_tool_calls: true,
            est_uncached_input_cost_1m_usd: None,
            est_cached_input_cost_1m_usd: None,
            est_cache_write_input_cost_1m_usd: None,
            est_output_cost_1m_usd: None,
            est_cache_storage_cost_1m_token_hour_usd: None,
        }],
        tags: Vec::new(),
        max_output_tokens: 0,
        transport: tau_provider_responses::Transport::Sse,
        compat: ResponsesCompat::default(),
    };
    let models = models_for_provider(&tau_proto::ProviderName::new("responses"), &provider);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id.to_string(), "responses/example-model");
    assert_eq!(
        models[0].supported_tool_types,
        vec![tau_proto::ToolType::Function]
    );
    assert_eq!(models[0].efforts, tau_proto::Effort::ALL.to_vec());
    assert!(!models[0].supports_compaction);
}

/// The shared canonical list must retain the documented UI order so default
/// public Responses capabilities and harness effort cycling cannot drift.
#[test]
fn canonical_responses_efforts_match_ui_order() {
    assert_eq!(
        tau_proto::Effort::ALL,
        [
            tau_proto::Effort::Off,
            tau_proto::Effort::Minimal,
            tau_proto::Effort::Low,
            tau_proto::Effort::Medium,
            tau_proto::Effort::High,
            tau_proto::Effort::XHigh,
            tau_proto::Effort::Max,
        ]
    );
}

/// Non-empty effort overrides are sets, so profile loading must canonicalize
/// their publication order instead of changing UI cycling based on input order.
#[test]
fn profile_canonicalizes_responses_effort_override() {
    let provider: ResponsesProvider = serde_json::from_value(serde_json::json!({
        "models": [{
            "id": "example-model",
            "efforts": ["max", "low", "off", "xhigh"]
        }]
    }))
    .expect("profile");

    assert_eq!(
        models_for_provider(&tau_proto::ProviderName::new("responses"), &provider)[0].efforts,
        vec![
            tau_proto::Effort::Off,
            tau_proto::Effort::Low,
            tau_proto::Effort::XHigh,
            tau_proto::Effort::Max,
        ]
    );
    assert_eq!(
        provider.models[0]
            .efforts
            .as_ref()
            .expect("configured effort override")
            .as_slice(),
        &[
            tau_proto::Effort::Off,
            tau_proto::Effort::Low,
            tau_proto::Effort::XHigh,
            tau_proto::Effort::Max,
        ]
    );
}

/// Duplicate configured effort names must fail profile loading rather than
/// quietly creating an ambiguous capability override.
#[test]
fn profile_rejects_duplicate_responses_efforts() {
    let error = serde_json::from_value::<ResponsesProvider>(serde_json::json!({
        "models": [{"id": "example-model", "efforts": ["low", "low"]}]
    }))
    .expect_err("duplicate efforts must fail");

    assert!(error.to_string().contains("must not contain duplicates"));
}

/// Direct Rust construction must retain the same duplicate rejection as
/// serialized profiles, so callers cannot publish an ambiguous effort set.
#[test]
fn responses_efforts_reject_duplicates() {
    assert!(
        ResponsesEfforts::try_from(vec![tau_proto::Effort::Low, tau_proto::Effort::Low]).is_err()
    );
}

/// An explicit empty effort override must publish no reasoning-effort
/// capability, which lets the harness clamp requests to its off value.
#[test]
fn profile_empty_responses_effort_override_disables_capability() {
    let provider: ResponsesProvider = serde_json::from_value(serde_json::json!({
        "models": [{"id": "example-model", "efforts": []}]
    }))
    .expect("profile");

    assert_eq!(
        models_for_provider(&tau_proto::ProviderName::new("responses"), &provider)[0].efforts,
        Vec::new()
    );
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
