//! Chat Completions extension ownership regression tests.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::num::NonZeroU64;
use std::{io as path_std_io, time as path_std_time};

use super::sampling::{RESPONSE_UPDATE_INTERVAL, ResponseSampler};
use super::*;

/// Qwen3.8 profiles publish only the model's exact selectable thinking levels
/// and retain literal `xhigh` lowering plus its first-system-only template
/// constraint through provider/model resolution.
#[test]
fn qwen_reasoning_profile_publishes_exact_efforts() {
    let provider: ChatCompletionsProvider = serde_json::from_value(serde_json::json!({
        "models": [{
            "id": "Qwen/Qwen3.8-27B",
            "context_window": 262144,
            "compat": {
                "reasoning_effort": {
                    "efforts": ["xhigh", "low", "medium"],
                    "wire": "literal"
                },
                "reasoning_replay": "both",
                "single_initial_system_message": true
            }
        }]
    }))
    .expect("Qwen profile");
    provider.validate().expect("valid Qwen profile");

    let models = models_for_provider(&tau_proto::ProviderName::new("qwen"), &provider);
    assert_eq!(
        models[0].efforts,
        vec![
            tau_proto::Effort::Low,
            tau_proto::Effort::Medium,
            tau_proto::Effort::XHigh,
        ]
    );
    let encoded = serde_json::to_value(&provider).expect("serialize Qwen profile");
    assert_eq!(
        encoded["models"][0]["compat"]["reasoning_effort"]["efforts"],
        serde_json::json!(["low", "medium", "xhigh"])
    );
    assert_eq!(
        encoded["models"][0]["compat"]["reasoning_replay"],
        serde_json::json!("both")
    );
}

/// Exact model-local image modality declarations must survive profile
/// serialization and model publication so only the audited route exposes
/// image-producing tools.
#[test]
fn image_tool_result_modalities_are_model_local_and_published() {
    let provider: ChatCompletionsProvider = serde_json::from_value(serde_json::json!({
        "models": [
            {
                "id": "Qwen/Qwen3.8-27B",
                "input_modalities": ["text", "image"],
                "tool_result_modalities": ["text", "image"]
            },
            {"id": "text-only"}
        ]
    }))
    .expect("vision profile");
    provider.validate().expect("valid paired capabilities");

    let models = models_for_provider(&tau_proto::ProviderName::new("ren"), &provider);
    assert_eq!(
        models[0].input_modalities,
        vec![
            tau_proto::InputModality::Text,
            tau_proto::InputModality::Image
        ]
    );
    assert_eq!(
        models[0].tool_result_modalities,
        vec![
            tau_proto::InputModality::Text,
            tau_proto::InputModality::Image
        ]
    );
    assert!(models[1].input_modalities.is_empty());
    assert!(models[1].tool_result_modalities.is_empty());

    let encoded = serde_json::to_value(provider).expect("profile serialization");
    assert_eq!(
        encoded["models"][0]["input_modalities"],
        serde_json::json!(["text", "image"])
    );
    assert!(encoded["models"][1].get("input_modalities").is_none());
}

/// Image capabilities are an atomic exact-route assertion: malformed order,
/// duplicates, image-only declarations, and one-sided declarations must fail
/// before model publication.
#[test]
fn image_tool_result_modalities_fail_closed() {
    for model in [
        serde_json::json!({
            "id": "one-sided",
            "input_modalities": ["text", "image"]
        }),
        serde_json::json!({
            "id": "image-only",
            "input_modalities": ["image"],
            "tool_result_modalities": ["image"]
        }),
        serde_json::json!({
            "id": "duplicate",
            "input_modalities": ["text", "image", "image"],
            "tool_result_modalities": ["text", "image"]
        }),
        serde_json::json!({
            "id": "reversed",
            "input_modalities": ["image", "text"],
            "tool_result_modalities": ["image", "text"]
        }),
    ] {
        let provider: ChatCompletionsProvider =
            serde_json::from_value(serde_json::json!({"models": [model]}))
                .expect("syntactically valid profile");
        assert!(provider.validate().is_err());
    }
}

/// Duplicate model ids would let publication and runtime resolution disagree
/// about image authority, so profiles must reject them even when only their
/// modality declarations differ.
#[test]
fn duplicate_model_ids_cannot_split_image_capability_authority() {
    let provider: ChatCompletionsProvider = serde_json::from_value(serde_json::json!({
        "models": [
            {
                "id": "same",
                "input_modalities": ["text", "image"],
                "tool_result_modalities": ["text", "image"]
            },
            {"id": "same"}
        ]
    }))
    .expect("syntactically valid profile");
    assert_eq!(
        provider.validate(),
        Err("Chat Completions model ids must be unique")
    );
}

/// OpenRouter's selected upstream is not an audited multimodal tool-result
/// route: declarations must fail validation and conversion must remain
/// text-only as defense in depth before publication or attempt resolution.
#[test]
fn openrouter_rejects_and_clears_image_capabilities() {
    let profile: OpenRouterProfile = serde_json::from_value(serde_json::json!({
        "models": [{
            "id": "upstream/model",
            "input_modalities": ["text", "image"],
            "tool_result_modalities": ["text", "image"]
        }]
    }))
    .expect("syntactically valid OpenRouter profile");
    assert_eq!(
        profile.validate(),
        Err("OpenRouter does not support image modality declarations")
    );

    let provider = profile.to_chat_completions();
    assert!(provider.models[0].input_modalities.is_empty());
    assert!(provider.models[0].tool_result_modalities.is_empty());
    let published = models_for_provider(&tau_proto::ProviderName::new("router"), &provider);
    assert!(published[0].input_modalities.is_empty());
    assert!(published[0].tool_result_modalities.is_empty());
}

/// Empty and duplicate effort sets are ambiguous publication contracts and
/// must fail while decoding the operator profile.
#[test]
fn reasoning_effort_profile_rejects_empty_and_duplicate_sets() {
    for (efforts, expected) in [
        (serde_json::json!([]), "must not be empty"),
        (
            serde_json::json!(["low", "low"]),
            "must not contain duplicates",
        ),
    ] {
        let error = serde_json::from_value::<ChatCompletionsProvider>(serde_json::json!({
            "models": [{
                "id": "model",
                "compat": {
                    "reasoning_effort": {
                        "efforts": efforts,
                        "wire": "literal"
                    }
                }
            }]
        }))
        .expect_err("invalid effort set");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

/// Omitted wire lowering can truthfully publish only one fixed server-side
/// effort because Tau cannot convey a per-turn selection.
#[test]
fn omitted_reasoning_effort_wire_requires_one_effective_effort() {
    let provider: ChatCompletionsProvider = serde_json::from_value(serde_json::json!({
        "models": [{
            "id": "llama-cpp-qwen",
            "compat": {
                "reasoning_effort": {
                    "efforts": ["low", "xhigh"],
                    "wire": "omit"
                }
            }
        }]
    }))
    .expect("syntactically valid profile");
    assert_eq!(
        provider.validate(),
        Err("omitted reasoning_effort wire requires exactly one effective effort")
    );
}

/// Cache controls must reject the retired boolean key flag so profiles cannot
/// accidentally retain GPT-5.6 implicit caching without an explicit policy.
#[test]
fn profile_rejects_retired_prompt_cache_key_flag() {
    let result = serde_json::from_value::<ChatCompletionsCompat>(serde_json::json!({
        "prompt_cache_key": true
    }));

    assert!(result.is_err());
}

/// A Chat Completions route may select only one typed OpenAI cache policy,
/// making the legacy automatic and explicit-boundary wire paths unambiguous.
#[test]
fn profile_validates_exclusive_openai_prompt_cache_policies() {
    let explicit: ChatCompletionsCompat = serde_json::from_value(serde_json::json!({
        "openai_prompt_cache": {
            "key": "agent",
            "options": {
                "mode": "explicit",
                "ttl": "30m",
                "boundary": "system_prompt"
            }
        }
    }))
    .expect("explicit policy");
    assert!(explicit.openai_prompt_cache.is_some());

    let ambiguous = serde_json::from_value::<ChatCompletionsCompat>(serde_json::json!({
        "openai_prompt_cache": {
            "key": "agent",
            "retention": "in_memory",
            "options": {
                "mode": "explicit",
                "ttl": "30m",
                "boundary": "system_prompt"
            }
        }
    }));
    assert!(ambiguous.is_err());

    let empty = serde_json::from_value::<ChatCompletionsCompat>(serde_json::json!({
        "openai_prompt_cache": {"key": "agent"}
    }));
    assert!(empty.is_err());
}

/// Cache telemetry must request the compatible stream usage field rather than
/// treating a server's undocumented default as a declared route capability.
#[test]
fn cache_usage_requires_stream_options() {
    let provider = ChatCompletionsProvider {
        compat: ChatCompletionsCompat {
            cache_usage: CacheUsageCompat::DeepSeek,
            ..ChatCompletionsCompat::default()
        },
        ..ChatCompletionsProvider::default()
    };
    assert_eq!(
        provider.validate(),
        Err("cache_usage requires stream_options")
    );

    let model_override = ChatCompletionsModel {
        id: tau_proto::ModelName::new("deepseek-v4-flash"),
        display_name: None,
        context_window: 128_000,
        compat: Some(ChatCompletionsCompat {
            cache_usage: CacheUsageCompat::DeepSeek,
            ..ChatCompletionsCompat::default()
        }),
        tags: Vec::new(),
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: true,
        local_summary_compaction: None,
        cache_contract: None,
        est_uncached_input_cost_1m_usd: None,
        est_cached_input_cost_1m_usd: None,
        est_cache_write_input_cost_1m_usd: None,
        est_output_cost_1m_usd: None,
        est_cache_storage_cost_1m_token_hour_usd: None,
    };
    let provider = ChatCompletionsProvider {
        models: vec![model_override],
        ..ChatCompletionsProvider::default()
    };
    assert_eq!(
        provider.validate(),
        Err("cache_usage requires stream_options")
    );
}

/// Ensures legacy model JSON omitting the additive capability remains
/// parallel-capable, preserving phase-1 profile compatibility.
#[test]
fn parallel_capability_defaults_true_and_is_omitted() {
    let model: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "local/model",
        "context_window": 4096
    }))
    .expect("legacy model");
    assert!(model.supports_parallel_tool_calls);
    let value = serde_json::to_value(model).expect("serialized model");
    assert!(value.get("supports_parallel_tool_calls").is_none());
}

/// Ensures generic compatible models receive default summary compaction while
/// a fully declared per-model profile overrides its limits.
#[test]
fn local_summary_compaction_defaults_without_model_profile() {
    let disabled: ChatCompletionsModel =
        serde_json::from_value(serde_json::json!({"id": "disabled"})).expect("disabled model");
    let enabled: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "enabled",
        "context_window": 8192,
        "local_summary_compaction": {
            "serialization_profile": "local_transcript_v1",
            "context_window_tokens": 8192,
            "max_input_bytes": 4096,
            "max_output_tokens": 512,
            "max_output_bytes": 4096
        }
    }))
    .expect("enabled model");
    let provider = ChatCompletionsProvider {
        models: vec![disabled, enabled],
        ..ChatCompletionsProvider::default()
    };

    let published = models_for_provider(&tau_proto::ProviderName::new("local"), &provider);
    assert!(published[0].supports_standalone_compaction);
    assert!(published[1].supports_standalone_compaction);
    assert!(
        published
            .iter()
            .all(|model| model.standalone_compaction_threshold.is_some())
    );
    assert!(published.iter().all(|model| !model.supports_compaction));

    let mut incompatible = provider.models[1].clone();
    incompatible
        .local_summary_compaction
        .as_mut()
        .expect("local profile")
        .context_window_tokens = NonZeroU64::new(4096).expect("positive");
    let incompatible_provider = ChatCompletionsProvider {
        models: vec![incompatible],
        ..ChatCompletionsProvider::default()
    };
    assert!(
        !models_for_provider(
            &tau_proto::ProviderName::new("local"),
            &incompatible_provider
        )[0]
        .supports_standalone_compaction
    );
}

/// Ensures malformed or unbounded summary opt-ins are rejected at profile load
/// rather than turning generic compatibility into compaction support.
#[test]
fn local_summary_compaction_profile_rejects_unknown_fields() {
    let error = serde_json::from_value::<ChatCompletionsModel>(serde_json::json!({
        "id": "invalid",
        "local_summary_compaction": {
            "serialization_profile": "local_transcript_v1",
            "context_window_tokens": 128000,
            "max_input_bytes": 1,
            "max_output_tokens": 1,
            "max_output_bytes": 1,
            "future_profile_guess": true
        }
    }))
    .expect_err("unknown summary profile field");
    assert!(error.to_string().contains("unknown field"));

    let zero = serde_json::from_value::<ChatCompletionsModel>(serde_json::json!({
        "id": "zero",
        "local_summary_compaction": {
            "serialization_profile": "local_transcript_v1",
            "context_window_tokens": 128000,
            "max_input_bytes": 1,
            "max_output_tokens": 0,
            "max_output_bytes": 1
        }
    }));
    assert!(zero.is_err(), "zero limits must fail profile decoding");
}

/// Ensures OpenRouter models retain explicit summary overrides and publish
/// standalone compaction like other Chat Completions routes.
#[test]
fn openrouter_enables_local_summary_compaction() {
    let model: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "remote",
        "context_window": 8192,
        "local_summary_compaction": {
            "serialization_profile": "local_transcript_v1",
            "context_window_tokens": 8192,
            "max_input_bytes": 4096,
            "max_output_tokens": 512,
            "max_output_bytes": 4096
        }
    }))
    .expect("configured remote model");
    let profile = OpenRouterProfile {
        api_key: String::new(),
        models: vec![model],
    };
    let provider = profile.to_chat_completions();

    assert!(provider.models[0].local_summary_compaction.is_some());
    assert!(
        models_for_provider(&tau_proto::ProviderName::new("openrouter"), &provider)[0]
            .supports_standalone_compaction
    );
}

/// OpenRouter enables only its documented streamed cache counters; unknown
/// upstream selection cannot create a cache-policy declaration or controls.
#[test]
fn openrouter_defaults_to_telemetry_without_cache_policy() {
    let model = ChatCompletionsModel {
        id: tau_proto::ModelName::new("upstream/model"),
        display_name: None,
        context_window: 128_000,
        compat: None,
        tags: Vec::new(),
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: true,
        local_summary_compaction: None,
        cache_contract: None,
        est_uncached_input_cost_1m_usd: None,
        est_cached_input_cost_1m_usd: None,
        est_cache_write_input_cost_1m_usd: None,
        est_output_cost_1m_usd: None,
        est_cache_storage_cost_1m_token_hour_usd: None,
    };
    let provider = OpenRouterProfile {
        api_key: String::new(),
        models: vec![model],
    }
    .to_chat_completions();

    assert!(provider.compat.stream_options);
    assert_eq!(provider.compat.cache_usage, CacheUsageCompat::OpenAi);
    assert!(provider.compat.openai_prompt_cache.is_none());
    let models = models_for_provider(&tau_proto::ProviderName::new("router"), &provider);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].cache_policy, None);
}

/// Ensures a provider output-token stop cannot commit a truncated checkpoint.
#[test]
fn run_prompt_attempt_terminalizes_truncated_local_summary() {
    let outcome = run_scripted_local_summary_attempt(concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Goal:\\ngoal\\nConstraints:\\nnone\\nDecisions:\\none\\nProgress:\\ndone\\nOpen Work:\\nnext\\nCritical Facts:\\nfact\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: [DONE]\n\n"
    ));
    assert!(matches!(
        outcome,
        super::attempt::PromptAttemptOutcome::Terminal { .. }
    ));
}

/// Ensures semantic output followed by a retryable provider failure terminates
/// without returning the standalone job to the retry scheduler.
#[test]
fn run_prompt_attempt_terminalizes_parsed_retryable_local_summary() {
    let outcome = run_scripted_local_summary_attempt(concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Goal:\\ngoal\"}}]}\n\n",
        "data: {\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"retry\"}}\n\n"
    ));
    assert!(matches!(
        outcome,
        super::attempt::PromptAttemptOutcome::Terminal { .. }
    ));
}

fn run_scripted_local_summary_attempt(
    events: &'static str,
) -> super::attempt::PromptAttemptOutcome {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let address = listener.local_addr().expect("fixture address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        let mut request = [0_u8; 16 * 1024];
        let _ = socket.read(&mut request).expect("read request");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            events.len(),
            events
        );
        socket
            .write_all(response.as_bytes())
            .expect("write response");
    });
    let model: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "local",
        "context_window": 8192,
        "local_summary_compaction": {
            "serialization_profile": "local_transcript_v1",
            "context_window_tokens": 8192,
            "max_input_bytes": 4096,
            "max_output_tokens": 512,
            "max_output_bytes": 4096
        }
    }))
    .expect("local model");
    let provider = ChatCompletionsProvider {
        base_url: format!("http://{address}/v1"),
        models: vec![model.clone()],
        ..ChatCompletionsProvider::default()
    };
    let prompt = tau_proto::AgentPromptCreated {
        agent_prompt_id: tau_proto::AgentPromptId::parse("ap-summary-test").expect("prompt id"),
        agent_id: tau_proto::AgentId::parse("agent-summary-test").expect("agent id"),
        session_id: tau_proto::SessionId::parse("session-summary-test").expect("session id"),
        system_prompt: String::new(),
        context: tau_proto::PromptContext {
            blocks: vec![tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![tau_proto::ContextItem::Message(tau_proto::MessageItem {
                        role: tau_proto::ContextRole::User,
                        content: vec![tau_proto::ContentPart::Text {
                            text: "history".to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: None,
                    })],
                },
            )],
        },
        tools: Vec::new(),
        tools_ref: None,
        model: tau_proto::ModelId::new(tau_proto::ProviderName::new("local"), model.id.clone()),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::None,
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
        operation: tau_proto::PromptOperation::StandaloneCompaction,
    };
    let mut bytes = Vec::new();
    let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
    let outcome = super::attempt::run_prompt_attempt(
        &prompt.agent_prompt_id,
        &prompt,
        &provider,
        &model,
        false,
        &mut writer,
        &mut || false,
        &tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None),
    );
    server.join().expect("join fixture");
    outcome
}

/// Explicit compatible-model prices validate as fixed-point decimals and
/// publish unchanged into provider model metadata.
#[test]
fn estimated_cost_prices_validate_and_publish() {
    let model: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "priced",
        "context_window": 4096,
        "est_uncached_input_cost_1m_usd": "2.5",
        "est_cached_input_cost_1m_usd": "0.25",
        "est_output_cost_1m_usd": 15
    }))
    .expect("priced model");
    let provider = ChatCompletionsProvider {
        models: vec![model],
        ..ChatCompletionsProvider::default()
    };

    let published = models_for_provider(&tau_proto::ProviderName::new("remote"), &provider);
    let rates = published[0].estimated_api_cost_rates();
    assert_eq!(rates.uncached_input.as_micro_usd(), 2_500_000);
    assert_eq!(rates.cached_input.as_micro_usd(), 250_000);
    assert_eq!(rates.output.as_micro_usd(), 15_000_000);

    for invalid in [
        serde_json::json!({
            "id": "negative",
            "est_uncached_input_cost_1m_usd": -1
        }),
        serde_json::json!({
            "id": "malformed",
            "est_output_cost_1m_usd": "free"
        }),
    ] {
        let error =
            serde_json::from_value::<ChatCompletionsModel>(invalid).expect_err("invalid price");
        assert!(error.to_string().contains("estimated USD price"));
    }
}

/// Unpriced compatible/local models resolve all categories through the
/// universal GPT-5.6 equivalent rather than presenting unavailable or free
/// pricing.
#[test]
fn unpriced_local_model_uses_central_fallback() {
    let provider = ChatCompletionsProvider {
        models: vec![ChatCompletionsModel {
            id: ModelName::new("local-free"),
            display_name: None,
            context_window: 4096,
            compat: None,
            tags: Vec::new(),
            input_modalities: Vec::new(),
            tool_result_modalities: Vec::new(),
            supports_parallel_tool_calls: true,
            local_summary_compaction: None,
            cache_contract: None,
            est_uncached_input_cost_1m_usd: None,
            est_cached_input_cost_1m_usd: None,
            est_cache_write_input_cost_1m_usd: None,
            est_output_cost_1m_usd: None,
            est_cache_storage_cost_1m_token_hour_usd: None,
        }],
        ..ChatCompletionsProvider::default()
    };

    let published = models_for_provider(&tau_proto::ProviderName::new("local"), &provider);

    assert_eq!(
        published[0].estimated_api_cost_rates(),
        tau_proto::ESTIMATED_API_COST_FALLBACK
    );
}

/// Each omitted price category falls back independently without replacing an
/// explicit category from the profile.
#[test]
fn partially_priced_model_uses_per_category_fallbacks() {
    let model: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "partial",
        "context_window": 4096,
        "est_cached_input_cost_1m_usd": "0.25"
    }))
    .expect("partially priced model");
    let provider = ChatCompletionsProvider {
        models: vec![model],
        ..ChatCompletionsProvider::default()
    };

    let published = models_for_provider(&tau_proto::ProviderName::new("remote"), &provider);
    let rates = published[0].estimated_api_cost_rates();

    assert_eq!(
        rates.uncached_input,
        tau_proto::ESTIMATED_API_COST_FALLBACK.uncached_input
    );
    assert_eq!(rates.cached_input.as_micro_usd(), 250_000);
    assert_eq!(rates.output, tau_proto::ESTIMATED_API_COST_FALLBACK.output);
}

/// Known compatible model ids without explicit profile prices resolve the
/// built-in default pricing instead of the central GPT-5.6-equivalent fallback.
#[test]
fn known_model_without_explicit_prices_uses_builtin_default() {
    let provider = ChatCompletionsProvider {
        models: vec![ChatCompletionsModel {
            id: ModelName::new("deepseek-v4-flash"),
            display_name: None,
            context_window: 4096,
            compat: None,
            tags: Vec::new(),
            input_modalities: Vec::new(),
            tool_result_modalities: Vec::new(),
            supports_parallel_tool_calls: true,
            local_summary_compaction: None,
            cache_contract: None,
            est_uncached_input_cost_1m_usd: None,
            est_cached_input_cost_1m_usd: None,
            est_cache_write_input_cost_1m_usd: None,
            est_output_cost_1m_usd: None,
            est_cache_storage_cost_1m_token_hour_usd: None,
        }],
        ..ChatCompletionsProvider::default()
    };

    let published = models_for_provider(&tau_proto::ProviderName::new("deepseek"), &provider);
    let rates = published[0].estimated_api_cost_rates();

    assert_eq!(rates.uncached_input.as_micro_usd(), 140_000);
    assert_eq!(rates.cached_input.as_micro_usd(), 2_800);
    assert_eq!(rates.output.as_micro_usd(), 280_000);
}

/// Explicit profile prices override the built-in default pricing for the same
/// model id.
#[test]
fn explicit_profile_prices_override_builtin_defaults() {
    let model: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "deepseek-v4-flash",
        "context_window": 4096,
        "est_uncached_input_cost_1m_usd": "1.5",
        "est_cached_input_cost_1m_usd": "0.05",
        "est_output_cost_1m_usd": "3"
    }))
    .expect("priced deepseek model");
    let provider = ChatCompletionsProvider {
        models: vec![model],
        ..ChatCompletionsProvider::default()
    };

    let published = models_for_provider(&tau_proto::ProviderName::new("deepseek"), &provider);
    let rates = published[0].estimated_api_cost_rates();

    assert_eq!(rates.uncached_input.as_micro_usd(), 1_500_000);
    assert_eq!(rates.cached_input.as_micro_usd(), 50_000);
    assert_eq!(rates.output.as_micro_usd(), 3_000_000);
}

/// Omitted profile categories on a known model id fall back to the built-in
/// default per category, never to the central GPT-5.6-equivalent fallback.
#[test]
fn partial_profile_prices_keep_builtin_defaults_for_omitted_categories() {
    let model: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "deepseek-v4-flash",
        "context_window": 4096,
        "est_output_cost_1m_usd": "9"
    }))
    .expect("partially priced deepseek model");
    let provider = ChatCompletionsProvider {
        models: vec![model],
        ..ChatCompletionsProvider::default()
    };

    let published = models_for_provider(&tau_proto::ProviderName::new("deepseek"), &provider);
    let rates = published[0].estimated_api_cost_rates();

    assert_eq!(rates.uncached_input.as_micro_usd(), 140_000);
    assert_eq!(rates.cached_input.as_micro_usd(), 2_800);
    assert_eq!(rates.output.as_micro_usd(), 9_000_000);
}

/// Ensures an explicit false publication capability survives serialization
/// independently from request-field compatibility.
#[test]
fn parallel_capability_false_is_independent_from_request_compatibility() {
    let provider = ChatCompletionsProvider {
        models: vec![ChatCompletionsModel {
            id: ModelName::new("serial-tools"),
            display_name: None,
            context_window: 4096,
            compat: Some(ChatCompletionsCompat {
                parallel_tool_calls: true,
                ..ChatCompletionsCompat::default()
            }),
            tags: Vec::new(),
            input_modalities: Vec::new(),
            tool_result_modalities: Vec::new(),
            supports_parallel_tool_calls: false,
            local_summary_compaction: None,
            cache_contract: None,
            est_uncached_input_cost_1m_usd: Default::default(),
            est_cached_input_cost_1m_usd: Default::default(),
            est_cache_write_input_cost_1m_usd: Default::default(),
            est_output_cost_1m_usd: Default::default(),
            est_cache_storage_cost_1m_token_hour_usd: None,
        }],
        ..ChatCompletionsProvider::default()
    };
    let published = models_for_provider(&tau_proto::ProviderName::new("local"), &provider);
    assert!(!published[0].supports_parallel_tool_calls);
    assert!(
        provider.models[0]
            .compat
            .expect("model compatibility")
            .parallel_tool_calls
    );
    assert_eq!(
        serde_json::to_value(&provider.models[0]).expect("model")["supports_parallel_tool_calls"],
        false
    );
}

/// Proves an operator-declared generic cache contract reaches transient model
/// metadata without deriving identity or lifecycle state from response samples.
#[test]
fn model_publishes_configured_runtime_cache_contract() {
    let model: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "cache-aware",
        "cache_contract": {
            "kind": "automatic_prefix",
            "ttl": {"kind": "sliding_known", "seconds": 300},
            "renewal": "read",
            "output_floor": "zero",
            "quota": {
                "requests": "counts_fully",
                "read_tokens": "exempt",
                "write_tokens": "counts_fully",
                "output_tokens": "exempt"
            },
            "privacy": {
                "storage": "volatile_memory",
                "zero_data_retention": "compatible",
                "data_residency": "preserves_route_policy",
                "manual_deletion": "unavailable"
            }
        }
    }))
    .expect("cache-aware model");
    let provider = ChatCompletionsProvider {
        models: vec![model],
        ..ChatCompletionsProvider::default()
    };

    let policy = models_for_provider(&tau_proto::ProviderName::new("generic"), &provider)[0]
        .cache_policy
        .expect("published cache policy");
    assert_eq!(policy.prefix_identity_version.get(), 1);
    assert!(matches!(
        policy.ttl,
        tau_proto::ProviderCacheTtl::SlidingKnown { seconds } if seconds.get() == 300
    ));
}

/// Proves a serialized generic Chat Completions profile decodes an opaque
/// externally managed cache reference while publishing Gemini explicit-object
/// policy and token-hour pricing without exposing that reference as model
/// state.
#[test]
fn gemini_explicit_object_profile_publishes_policy_without_lifecycle_state() {
    let provider: ChatCompletionsProvider = serde_json::from_value(serde_json::json!({
        "extra_body": {
            "cached_content": "cachedContents/external-owner-object"
        },
        "models": [
            {
                "id": "gemini-2.5-flash-explicit",
                "cache_contract": {
                    "kind": "explicit_object",
                    "ttl": {"kind": "fixed", "seconds": 3600},
                    "renewal": "patch_expiry",
                    "output_floor": "zero",
                    "quota": {
                        "requests": "unknown",
                        "read_tokens": "unknown",
                        "write_tokens": "unknown",
                        "output_tokens": "unknown"
                    },
                    "privacy": {
                        "storage": "named_provider_object",
                        "zero_data_retention": "incompatible",
                        "data_residency": "provider_specific",
                        "manual_deletion": "unavailable"
                    }
                },
                "est_cache_storage_cost_1m_token_hour_usd": "1"
            },
            {
                "id": "gemini-2.5-flash-implicit",
                "cache_contract": {
                    "kind": "automatic_prefix",
                    "ttl": {"kind": "unknown"},
                    "renewal": "unsupported",
                    "output_floor": "unknown",
                    "quota": {
                        "requests": "unknown",
                        "read_tokens": "unknown",
                        "write_tokens": "unknown",
                        "output_tokens": "unknown"
                    },
                    "privacy": {
                        "storage": "unknown",
                        "zero_data_retention": "unknown",
                        "data_residency": "unknown",
                        "manual_deletion": "unavailable"
                    }
                }
            }
        ]
    }))
    .expect("Gemini explicit object profile");

    assert_eq!(
        provider.extra_body["cached_content"],
        serde_json::json!("cachedContents/external-owner-object")
    );
    let published = models_for_provider(&tau_proto::ProviderName::new("generic-gemini"), &provider);
    let explicit_model = published.first().expect("published explicit model");
    let policy = explicit_model.cache_policy.expect("published cache policy");
    assert_eq!(
        (
            policy.kind,
            policy.renewal,
            policy.output_floor,
            policy.privacy.storage,
            policy.privacy.manual_deletion,
        ),
        (
            tau_proto::ProviderCacheKind::ExplicitObject,
            tau_proto::ProviderCacheRenewal::PatchExpiry,
            tau_proto::ProviderCacheOutputFloor::Zero,
            tau_proto::ProviderCacheStorageMode::NamedProviderObject,
            tau_proto::ProviderCacheDeletionAvailability::Unavailable,
        )
    );
    assert!(matches!(
        policy.ttl,
        tau_proto::ProviderCacheTtl::Fixed { seconds } if seconds.get() == 3_600
    ));
    assert_eq!(
        policy.quota,
        tau_proto::ProviderCacheQuotaAccounting {
            requests: tau_proto::ProviderCacheQuotaCharge::Unknown,
            read_tokens: tau_proto::ProviderCacheQuotaCharge::Unknown,
            write_tokens: tau_proto::ProviderCacheQuotaCharge::Unknown,
            output_tokens: tau_proto::ProviderCacheQuotaCharge::Unknown,
        }
    );
    assert_eq!(
        (
            policy.privacy.zero_data_retention,
            policy.privacy.data_residency,
        ),
        (
            tau_proto::ProviderCacheZeroDataRetentionCompatibility::Incompatible,
            tau_proto::ProviderCacheDataResidencyEffect::ProviderSpecific,
        )
    );
    assert_eq!(
        explicit_model
            .est_cache_storage_cost_1m_token_hour_usd
            .expect("published storage price")
            .as_micro_usd(),
        1_000_000
    );
    let serialized =
        serde_json::to_value(explicit_model).expect("serialize published explicit model");
    assert!(
        !serialized.to_string().contains("external-owner-object"),
        "runtime model metadata must not carry the configured object reference"
    );
    let implicit_policy = published
        .get(1)
        .expect("published implicit model")
        .cache_policy
        .expect("published implicit policy");
    assert_eq!(
        (
            implicit_policy.kind,
            implicit_policy.ttl,
            implicit_policy.renewal,
            implicit_policy.output_floor,
        ),
        (
            tau_proto::ProviderCacheKind::AutomaticPrefix,
            tau_proto::ProviderCacheTtl::Unknown,
            tau_proto::ProviderCacheRenewal::Unsupported,
            tau_proto::ProviderCacheOutputFloor::Unknown,
        )
    );
}

/// Proves the generic Chat Completions config owner publishes GPT-5.6's
/// documented minimum and prices while keeping an older model's legacy request
/// control and conservative unknown residency in a separate exact-route policy.
#[test]
fn openai_model_configs_publish_distinct_cache_policies() {
    let gpt_5_6: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "gpt-5.6-sol",
        "compat": {
            "cache_usage": "open_ai",
            "openai_prompt_cache": {
                "key": "agent",
                "options": {
                    "mode": "explicit",
                    "ttl": "30m",
                    "boundary": "system_prompt"
                }
            }
        },
        "cache_contract": {
            "kind": "explicit_breakpoint",
            "ttl": {"kind": "minimum", "seconds": 1800},
            "renewal": "unsupported",
            "output_floor": "unknown",
            "quota": {
                "requests": "unknown",
                "read_tokens": "unknown",
                "write_tokens": "unknown",
                "output_tokens": "unknown"
            },
            "privacy": {
                "storage": "unknown",
                "zero_data_retention": "unknown",
                "data_residency": "unknown",
                "manual_deletion": "unavailable"
            }
        },
        "est_uncached_input_cost_1m_usd": "5",
        "est_cached_input_cost_1m_usd": "0.50",
        "est_cache_write_input_cost_1m_usd": "6.25"
    }))
    .expect("GPT-5.6 explicit cache model");
    let older: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "gpt-5.5",
        "compat": {
            "openai_prompt_cache": {
                "key": "agent",
                "retention": "24h"
            }
        },
        "cache_contract": {
            "kind": "automatic_prefix",
            "ttl": {"kind": "unknown"},
            "renewal": "unsupported",
            "output_floor": "unknown",
            "quota": {
                "requests": "unknown",
                "read_tokens": "unknown",
                "write_tokens": "unknown",
                "output_tokens": "unknown"
            },
            "privacy": {
                "storage": "unknown",
                "zero_data_retention": "unknown",
                "data_residency": "unknown",
                "manual_deletion": "unavailable"
            }
        }
    }))
    .expect("older automatic cache model");
    assert!(matches!(
        gpt_5_6
            .compat
            .expect("GPT-5.6 compatibility")
            .openai_prompt_cache
            .expect("GPT-5.6 cache request control")
            .policy,
        OpenAiPromptCachePolicy::Explicit { .. }
    ));
    assert!(matches!(
        older
            .compat
            .expect("older compatibility")
            .openai_prompt_cache
            .expect("older cache request control")
            .policy,
        OpenAiPromptCachePolicy::Legacy { .. }
    ));

    let provider = ChatCompletionsProvider {
        models: vec![gpt_5_6, older],
        ..ChatCompletionsProvider::default()
    };
    let published = models_for_provider(&tau_proto::ProviderName::new("openai"), &provider);
    let gpt_5_6 = &published[0];
    let gpt_policy = gpt_5_6.cache_policy.expect("published GPT-5.6 policy");
    assert_eq!(
        gpt_policy.kind,
        tau_proto::ProviderCacheKind::ExplicitBreakpoint
    );
    assert!(matches!(
        gpt_policy.ttl,
        tau_proto::ProviderCacheTtl::Minimum { seconds } if seconds.get() == 1_800
    ));
    assert_eq!(
        gpt_policy.renewal,
        tau_proto::ProviderCacheRenewal::Unsupported
    );
    assert_eq!(
        (
            gpt_5_6
                .est_uncached_input_cost_1m_usd
                .expect("published uncached price")
                .as_micro_usd(),
            gpt_5_6
                .est_cached_input_cost_1m_usd
                .expect("published cache-read price")
                .as_micro_usd(),
            gpt_5_6
                .est_cache_write_input_cost_1m_usd
                .expect("published cache-write price")
                .as_micro_usd(),
        ),
        (5_000_000, 500_000, 6_250_000)
    );

    let older_policy = published[1]
        .cache_policy
        .expect("published older-model policy");
    assert_eq!(
        older_policy.kind,
        tau_proto::ProviderCacheKind::AutomaticPrefix
    );
    assert_eq!(older_policy.ttl, tau_proto::ProviderCacheTtl::Unknown);
    assert_eq!(
        older_policy.renewal,
        tau_proto::ProviderCacheRenewal::Unsupported
    );
}

/// Ensures the extension-owned sampler preserves the successful append-only
/// event cadence: first semantic output immediately, later output batched,
/// and an immediate terminal flush with chained stats.
#[test]
fn successful_sampling_preserves_delta_and_stats_order() {
    let prompt = crate::openai_tests::prompt();
    let apid = prompt.agent_prompt_id.clone();
    let started_at = path_std_time::Instant::now();
    let mut sampler = ResponseSampler::new();
    sampler.started_at = started_at;
    sampler.latest_items = vec![assistant_message(0, "hel")];
    sampler.latest_bytes = 3;
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        sampler.emit_at(&apid, &prompt, &mut writer, started_at, false);
        sampler.latest_items = vec![assistant_message(0, "hello")];
        sampler.latest_bytes = 5;
        sampler.emit_at(
            &apid,
            &prompt,
            &mut writer,
            started_at + RESPONSE_UPDATE_INTERVAL / 2,
            false,
        );
        sampler.emit_at(
            &apid,
            &prompt,
            &mut writer,
            started_at + RESPONSE_UPDATE_INTERVAL / 2,
            true,
        );
    }
    let frames = decode_frames(&bytes);
    assert_eq!(frames.len(), 2);
    let updates = frames
        .into_iter()
        .map(|frame| {
            let tau_proto::HarnessInputMessage::Emit(emit) = frame else {
                panic!("expected emitted provider update");
            };
            let tau_proto::Event::ProviderResponseUpdatedReported(update) = *emit.event else {
                panic!("expected provider response update");
            };
            update
        })
        .collect::<Vec<_>>();
    assert_eq!(
        updates[0].deltas,
        vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "hel".to_owned(),
            phase: None,
        }]
    );
    assert_eq!(
        updates[1].deltas,
        vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "lo".to_owned(),
            phase: None,
        }]
    );
    let first = updates[0].response_stats.expect("first stats");
    let terminal = updates[1].response_stats.expect("terminal stats");
    assert_eq!(first.previous.response_bytes_received, 0);
    assert_eq!(first.current.response_bytes_received, 3);
    assert_eq!(terminal.previous, first.current);
    assert_eq!(terminal.current.response_bytes_received, 5);
}

/// Ensures late materialization of an earlier tool slot cannot shift and
/// duplicate text already emitted at its stable backend index.
#[test]
fn stable_output_indices_prevent_duplicate_text_after_late_tool_metadata() {
    let mut sampler = ResponseSampler::new();
    sampler.latest_items = vec![assistant_message(1, "hello")];
    assert_eq!(sampler.deltas().len(), 1);
    sampler.latest_items = vec![
        tau_provider_chat_completions::AttemptOutputItem {
            output_index: 0,
            item: tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("lookup"),
                tool_type: tau_proto::ToolType::Function,
                arguments: tau_proto::CborValue::Map(Vec::new()),
                raw_arguments_json: Some("{}".to_owned()),
                responses_envelope: None,
            }),
        },
        assistant_message(1, "hello"),
    ];
    assert!(sampler.deltas().is_empty());
}

/// Ensures content-free tool progress still publishes response-byte stats
/// without fabricating text deltas.
#[test]
fn stats_only_tool_bytes_emit_without_text_delta() {
    let prompt = crate::openai_tests::prompt();
    let mut sampler = ResponseSampler::new();
    sampler.latest_bytes = 17;
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        sampler.emit_at(
            &prompt.agent_prompt_id,
            &prompt,
            &mut writer,
            sampler.started_at,
            false,
        );
    }
    let updates = decode_updates(&bytes);
    assert_eq!(updates.len(), 1);
    assert!(updates[0].deltas.is_empty());
    assert_eq!(
        updates[0]
            .response_stats
            .expect("stats")
            .current
            .response_bytes_received,
        17
    );
}

/// Ensures a due zero-byte elapsed sample is emitted and later first bytes
/// bypass the next cadence deadline exactly once.
#[test]
fn due_zero_bytes_then_first_bytes_after_idle_preserve_cadence() {
    let prompt = crate::openai_tests::prompt();
    let started_at = path_std_time::Instant::now();
    let mut sampler = ResponseSampler::new();
    sampler.started_at = started_at;
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        sampler.emit_at(
            &prompt.agent_prompt_id,
            &prompt,
            &mut writer,
            started_at + RESPONSE_UPDATE_INTERVAL,
            false,
        );
        sampler.latest_bytes = 5;
        sampler.emit_at(
            &prompt.agent_prompt_id,
            &prompt,
            &mut writer,
            started_at + RESPONSE_UPDATE_INTERVAL + RESPONSE_UPDATE_INTERVAL / 2,
            false,
        );
    }
    let updates = decode_updates(&bytes);
    assert_eq!(updates.len(), 2);
    assert_eq!(
        updates[0]
            .response_stats
            .as_ref()
            .expect("zero stats")
            .current
            .response_bytes_received,
        0
    );
    assert_eq!(
        updates[1]
            .response_stats
            .as_ref()
            .expect("first bytes")
            .current
            .response_bytes_received,
        5
    );
}

/// Ensures reasoning and message deltas retain independent stable indices, and
/// non-prefix provider corrections are not re-emitted as duplicate text.
#[test]
fn reasoning_multi_index_and_non_prefix_correction_are_stable() {
    let mut sampler = ResponseSampler::new();
    sampler.latest_items = vec![
        assistant_message(1, "hello"),
        tau_provider_chat_completions::AttemptOutputItem {
            output_index: 3,
            item: tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "why".to_owned(),
            }),
        },
    ];
    let first = sampler.deltas();
    assert_eq!(first.len(), 2);
    sampler.latest_items = vec![
        assistant_message(1, "replacement"),
        tau_provider_chat_completions::AttemptOutputItem {
            output_index: 3,
            item: tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "why now".to_owned(),
            }),
        },
    ];
    assert_eq!(
        sampler.deltas(),
        vec![tau_proto::ProviderResponseTextDelta::ReasoningText {
            output_index: 3,
            kind: tau_proto::ReasoningTextKind::Full,
            text: " now".to_owned(),
        }]
    );
}

fn assistant_message(
    output_index: u32,
    text: &str,
) -> tau_provider_chat_completions::AttemptOutputItem {
    tau_provider_chat_completions::AttemptOutputItem {
        output_index,
        item: tau_proto::ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::Assistant,
            content: vec![tau_proto::ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        }),
    }
}

fn decode_frames(bytes: &[u8]) -> Vec<tau_proto::HarnessInputMessage> {
    let mut reader = tau_proto::HarnessInputReader::new(path_std_io::BufReader::new(bytes));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("decode frame") {
        frames.push(frame);
    }
    frames
}

fn decode_updates(bytes: &[u8]) -> Vec<tau_proto::ProviderResponseUpdated> {
    decode_frames(bytes)
        .into_iter()
        .map(|frame| {
            let tau_proto::HarnessInputMessage::Emit(emit) = frame else {
                panic!("expected emitted update");
            };
            let tau_proto::Event::ProviderResponseUpdatedReported(update) = *emit.event else {
                panic!("expected provider response update");
            };
            update
        })
        .collect()
}
