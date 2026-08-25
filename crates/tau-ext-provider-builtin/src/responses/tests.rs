use super::*;

/// Public Responses accepts legacy retention and its one opt-in typed
/// input-text boundary while rejecting the Chat Completions boundary.
#[test]
fn profile_accepts_supported_cache_policies_and_rejects_wrong_boundary() {
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
            policy: OpenAiPromptCachePolicy::Legacy {
                retention: crate::OpenAiPromptCacheRetention::Hours24,
            },
        })
    );

    let explicit: ResponsesProvider = serde_json::from_value(serde_json::json!({
        "compat": {
            "openai_prompt_cache": {
                "key": "agent",
                "options": {
                    "mode": "explicit",
                    "ttl": "30m",
                    "boundary": "first_input_text"
                }
            }
        }
    }))
    .expect("explicit cache profile");
    assert!(matches!(
        explicit.compat.openai_prompt_cache,
        Some(OpenAiPromptCache {
            policy: OpenAiPromptCachePolicy::Explicit { .. },
            ..
        })
    ));

    let unsupported = serde_json::from_value::<ResponsesProvider>(serde_json::json!({
        "compat": {"openai_prompt_cache": {"key": "agent", "options": {
            "mode": "explicit", "ttl": "30m", "boundary": "system_prompt"
        }}}
    }));
    assert!(unsupported.is_err());

    for invalid in [
        serde_json::json!({"key": "agent"}),
        serde_json::json!({"key": "agent", "retention": "24h", "options": {
            "mode": "explicit", "ttl": "30m", "boundary": "first_input_text"
        }}),
    ] {
        assert!(
            serde_json::from_value::<ResponsesProvider>(serde_json::json!({
                "compat": {"openai_prompt_cache": invalid}
            }))
            .is_err()
        );
    }
}

/// Hydrated Responses provider serialization must retain its explicit route and
/// model list rather than gaining discovery or an implicit model; this does not
/// cover credential-free persisted settings.
#[test]
fn profile_round_trips_explicit_models() {
    let profile: ResponsesProvider = serde_json::from_value(serde_json::json!({
        "base_url": "https://example.test/v1",
        "api_key": "fixture-api-key",
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
            context_window: 128_000,
            tags: Vec::new(),
            supports_parallel_tool_calls: true,
            local_summary_compaction: None,
            cache_contract: None,
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
    assert_eq!(
        models[0].efforts,
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
    assert!(!models[0].supports_compaction);
    assert!(models[0].supports_standalone_compaction);
    assert_eq!(models[0].standalone_compaction_threshold, Some(30_720));
}

/// Public Responses emits both visible reasoning text and an opaque replay
/// item; summary validation must discard both while retaining one narrative.
#[test]
fn summary_validation_accepts_responses_dual_reasoning_representation() {
    let items = vec![
        tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text: "bounded thought".to_owned(),
        }),
        tau_proto::ContextItem::Reasoning(tau_proto::OpaqueProviderItem {
            value: tau_proto::json_to_cbor(
                &serde_json::json!({"type": "reasoning", "id": "reasoning-1"}),
            ),
            raw_json: Some(r#"{"type":"reasoning","id":"reasoning-1"}"#.to_owned()),
        }),
        tau_proto::ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::Assistant,
            content: vec![tau_proto::ContentPart::Text {
                text: "useful checkpoint".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        }),
    ];
    let config = SummaryCompactionConfig::default_for(128_000).expect("ordinary context");
    assert!(matches!(
        validate_responses_narrative_output(items, config),
        Ok(tau_proto::ContextItem::LocalCompactionNarrative(_))
    ));
}

/// Public Responses summary dispatch must retain the ordinary request and add
/// only the harness-authored instruction after its warmed context prefix.
#[test]
fn summary_prompt_appends_instruction_to_ordinary_prefix() {
    let mut prompt = tau_proto::AgentPromptCreated {
        agent_prompt_id: "responses-summary".parse().expect("prompt id"),
        agent_id: tau_proto::AgentId::parse("responses-summary").expect("agent id"),
        session_id: "responses-summary".parse().expect("session id"),
        system_prompt: "ordinary authority".to_owned(),
        context: tau_proto::PromptContext {
            blocks: vec![tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![tau_proto::ContextItem::CompactionTrigger],
                },
            )],
        },
        tools: vec![tau_proto::ToolDefinition {
            name: tau_proto::ToolName::new("dangerous"),
            model_visible_name: None,
            description: None,
            tool_type: tau_proto::ToolType::Function,
            parameters: None,
            format: None,
        }],
        tools_ref: None,
        model: "responses/test".parse().expect("model"),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
        operation: tau_proto::PromptOperation::StandaloneCompaction,
    };
    prompt.tools_ref = Some(tau_proto::PromptToolsRef {
        base_agent_prompt_id: "old-tools".parse().expect("prompt id"),
    });
    let config = SummaryCompactionConfig::default_for(128_000).expect("ordinary context");
    let compact = materialize_summary_prompt(&prompt, config).expect("summary prompt");
    assert_eq!(compact.system_prompt, prompt.system_prompt);
    assert_eq!(compact.context.blocks.len(), 1);
    assert_eq!(compact.tools, prompt.tools);
    assert_eq!(compact.tools_ref, prompt.tools_ref);
    assert_eq!(compact.tool_choice, prompt.tool_choice);
    let tau_proto::ContextBlock::UserInput(block) = &compact.context.blocks[0] else {
        panic!("trailing instruction must be user input");
    };
    let [tau_proto::ContextItem::Message(message)] = block.items.as_slice() else {
        panic!("trailing instruction must be one message");
    };
    assert_eq!(message.role, tau_proto::ContextRole::User);
    assert_eq!(
        message.content,
        [tau_proto::ContentPart::Text {
            text: tau_provider::local_summary_compaction::REQUEST.to_owned()
        }]
    );
}

/// Summary work may retry before semantic output, but any semantic progress
/// makes redispatch ambiguous and therefore terminal.
#[test]
fn summary_retry_policy_terminalizes_only_after_semantic_output() {
    let progress = |has_timed_semantic_output| tau_provider_responses::AttemptProgress {
        output_items: Vec::new(),
        response_bytes_received: 0,
        has_timed_semantic_output,
    };
    assert!(!summary_retry_is_terminal(true, &progress(false)));
    assert!(summary_retry_is_terminal(true, &progress(true)));
    assert!(!summary_retry_is_terminal(false, &progress(true)));
    let config = SummaryCompactionConfig::default_for(128_000).expect("ordinary context");
    assert_eq!(attempt_output_tokens(99, Some(config), true), 4096);
}

/// Proves public Responses models publish explicitly configured cache metadata
/// without changing their request controls or creating lifecycle state.
#[test]
fn profile_publishes_configured_runtime_cache_contract() {
    let model: ResponsesModel = serde_json::from_value(serde_json::json!({
        "id": "cache-aware",
        "cache_contract": {
            "kind": "explicit_breakpoint",
            "ttl": {"kind": "minimum", "seconds": 1800},
            "renewal": "recreate",
            "output_floor": "unbounded_reasoning",
            "quota": {
                "requests": "counts_fully",
                "read_tokens": "counts_fully",
                "write_tokens": "counts_fully",
                "output_tokens": "provider_specific"
            },
            "privacy": {
                "storage": "extended_provider_retention",
                "zero_data_retention": "provider_specific",
                "data_residency": "provider_specific",
                "manual_deletion": "unavailable"
            }
        }
    }))
    .expect("cache-aware Responses model");
    let provider = ResponsesProvider {
        models: vec![model],
        ..ResponsesProvider::default()
    };

    let policy = models_for_provider(&tau_proto::ProviderName::new("responses"), &provider)[0]
        .cache_policy
        .expect("published cache policy");
    assert_eq!(
        policy.kind,
        tau_proto::ProviderCacheKind::ExplicitBreakpoint
    );
    assert_eq!(
        policy.ttl,
        tau_proto::ProviderCacheTtl::Minimum {
            seconds: std::num::NonZeroU64::new(1_800).expect("positive test duration")
        }
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
/// The complete extension attempt path must pass both durable-session policy
/// values unchanged at the actual generic Responses adapter invocation.
#[test]
fn debug_capture_policy_is_forwarded_to_generic_responses() {
    let prompt = tau_proto::AgentPromptCreated {
        agent_prompt_id: "responses-forwarding".parse().expect("prompt id"),
        agent_id: tau_proto::AgentId::parse("responses-forwarding").expect("agent id"),
        session_id: "responses-forwarding".parse().expect("session id"),
        system_prompt: String::new(),
        context: tau_proto::PromptContext::default(),
        tools: Vec::new(),
        tools_ref: None,
        model: "responses/test".parse().expect("model id"),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
        operation: tau_proto::PromptOperation::Inference,
    };
    let provider = ResponsesProvider {
        base_url: "not a URL".to_owned(),
        ..ResponsesProvider::default()
    };
    let model: ResponsesModel =
        serde_json::from_value(serde_json::json!({"id": "test-model"})).expect("model");
    let network = tau_provider::OutboundNetworkPolicy::from_environment(Default::default(), None);
    let mut bytes = Vec::new();
    let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
    for enabled in [true, false] {
        let _ = run_prompt_attempt(
            &prompt.agent_prompt_id,
            &prompt,
            &provider,
            &model,
            enabled,
            &mut writer,
            &mut || false,
            &network,
        );
    }
    assert_eq!(super::take_forwarded_debug_capture_policy(), [true, false]);
}
