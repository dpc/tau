use std::num::{NonZeroU32, NonZeroU64};
use std::{
    io as path_std_io, net as path_std_net, thread as path_std_thread, time as path_std_time,
};

use super::sampling::ResponsesResponseSampler;
use super::*;

/// Minimal borrowed projection used to exercise the production sampler seam.
struct FakeSamplingProgress<'a> {
    /// Cumulative byte count.
    bytes: u64,
    /// Assistant text.
    message: &'a str,
    /// Reasoning text.
    reasoning: &'a str,
}

impl sampling::SamplingProgress for FakeSamplingProgress<'_> {
    fn response_bytes_received(&self) -> u64 {
        self.bytes
    }

    fn has_timed_semantic_output(&self) -> bool {
        true
    }

    fn visit_display_output(
        &self,
        visit: &mut dyn FnMut(
            u32,
            tau_provider_responses::DisplayOutputKind,
            &str,
            tau_provider_responses::DisplayGeneration,
        ),
    ) {
        visit(
            0,
            tau_provider_responses::DisplayOutputKind::Message,
            self.message,
            Default::default(),
        );
        visit(
            1,
            tau_provider_responses::DisplayOutputKind::Reasoning,
            self.reasoning,
            Default::default(),
        );
    }
}

/// Disabled output-cost diagnostics must not add a delta-byte traversal to the
/// production sampler path.
#[test]
fn disabled_output_cost_sampler_performs_no_diagnostic_traversal() {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("off")
        .without_time()
        .with_writer(path_std_io::sink)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        crate::output_cost_observation::reset_diagnostic_traversals();
        let prompt = crate::openai_tests::prompt();
        let mut sampler = ResponsesResponseSampler::new();
        sampler.latest_bytes = 1;
        let mut output = Vec::new();
        let mut writer = tau_proto::PeerOutputWriter::new(&mut output);
        sampler.emit_at(
            &prompt.agent_prompt_id,
            &prompt,
            &mut writer,
            path_std_time::Instant::now(),
            true,
        );
        assert_eq!(crate::output_cost_observation::diagnostic_traversals(), 0);
    });
}

/// The production due-sample seam publishes borrowed message/reasoning
/// projections and slices multibyte suffixes from byte cursors.
#[test]
fn due_borrowed_projection_publishes_initial_and_unicode_suffix_deltas() {
    let prompt = crate::openai_tests::prompt();
    let mut sampler = ResponsesResponseSampler::new();
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        sampler.emit_if_due_from(
            &prompt.agent_prompt_id,
            &prompt,
            &FakeSamplingProgress {
                bytes: 2,
                message: "a",
                reasoning: "r",
            },
            &mut writer,
        );
        sampler.last_emitted_at =
            Some(path_std_time::Instant::now() - sampling::RESPONSE_UPDATE_INTERVAL);
        sampler.emit_if_due_from(
            &prompt.agent_prompt_id,
            &prompt,
            &FakeSamplingProgress {
                bytes: 7,
                message: "a雪",
                reasoning: "rλ",
            },
            &mut writer,
        );
    }
    let mut reader =
        tau_proto::HarnessInputReader::new(path_std_io::BufReader::new(bytes.as_slice()));
    let mut updates = Vec::new();
    while let Some(message) = reader.read_message().expect("decode response update") {
        if let tau_proto::HarnessInputMessage::Emit(emit) = message
            && let tau_proto::Event::ProviderResponseUpdatedReported(update) = *emit.event
        {
            updates.push(update);
        }
    }
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].deltas.len(), 2);
    assert_eq!(
        updates[1].deltas,
        vec![
            tau_proto::ProviderResponseTextDelta::Message {
                output_index: 0,
                text: "雪".to_owned(),
                phase: None,
            },
            tau_proto::ProviderResponseTextDelta::ReasoningText {
                output_index: 1,
                kind: tau_proto::ReasoningTextKind::Full,
                text: "λ".to_owned(),
            },
        ]
    );
}

/// Generic public Responses reports must carry the distinct full-replay backend
/// kind so private Codex cannot acquire their continuation authority.
#[test]
fn finished_response_uses_public_responses_backend_kind() {
    let prompt = crate::openai_tests::prompt();
    let provider = ResponsesProvider {
        base_url: "https://api.openai.com/v1".to_owned(),
        ..ResponsesProvider::default()
    };
    let response = finished(
        &prompt.agent_prompt_id,
        &prompt,
        &provider,
        Vec::new(),
        tau_proto::ProviderStopReason::Length,
        None,
        None,
        None,
        None,
        true,
        tau_proto::ProviderAttempt::ONE,
    );
    assert_eq!(
        response.backend.expect("backend").kind,
        tau_proto::ProviderBackendKind::PublicResponses
    );
}

/// The production public Responses adapter omits backend identity before
/// dispatch and retains it for both failed and successful reached requests.
#[test]
fn production_attempt_backend_metadata_tracks_actual_egress() {
    let prompt = crate::openai_tests::prompt();
    let model: ResponsesModel =
        serde_json::from_value(serde_json::json!({"id": "test-model"})).expect("model");
    let network = tau_provider::OutboundNetworkPolicy::from_environment(Default::default(), None);
    let mut bytes = Vec::new();
    let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
    let pre_egress = run_prompt_attempt(
        &prompt.agent_prompt_id,
        &prompt,
        &ResponsesProvider {
            base_url: "not a URL".to_owned(),
            ..ResponsesProvider::default()
        },
        &model,
        false,
        &mut writer,
        &mut || false,
        &network,
        tau_proto::ProviderAttempt::ONE,
    );
    assert!(matches!(
        pre_egress,
        PromptAttemptOutcome::Retry {
            backend_reached: false,
            ..
        }
    ));

    let failed = run_loopback_attempt(
        &prompt,
        &model,
        "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: 63\r\nconnection: close\r\n\r\n{\"error\":{\"code\":\"invalid_request_error\",\"message\":\"rejected\"}}",
    );
    assert!(matches!(
        failed,
        PromptAttemptOutcome::Terminal { finished, .. }
            if finished.backend.as_ref().is_some_and(|backend| {
                backend.kind == tau_proto::ProviderBackendKind::PublicResponses
            })
    ));

    let auth = run_loopback_attempt(
        &prompt,
        &model,
        "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: 36\r\nconnection: close\r\n\r\n{\"error\":{\"code\":\"invalid_api_key\"}}",
    );
    assert!(matches!(
        auth,
        PromptAttemptOutcome::Retry {
            decision,
            backend_reached: true,
            ..
        } if decision.class == tau_provider::retry_policy::RetryClass::Auth
    ));

    let body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_ok\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}]}}\n\n";
    let succeeded = run_loopback_attempt(
        &prompt,
        &model,
        &format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(matches!(
        succeeded,
        PromptAttemptOutcome::Finished(finished)
            if finished.backend.as_ref().is_some_and(|backend| {
                backend.kind == tau_proto::ProviderBackendKind::PublicResponses
            })
    ));
}

/// Run the production public Responses adapter against one finite loopback
/// response.
fn run_loopback_attempt(
    prompt: &tau_proto::AgentPromptCreated,
    model: &ResponsesModel,
    response: &str,
) -> PromptAttemptOutcome {
    use path_std_io::{Read as _, Write as _};

    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("loopback address");
    let response = response.to_owned();
    let server = path_std_thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).expect("read request");
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });
    let provider = ResponsesProvider {
        base_url: format!("http://{address}/v1"),
        ..ResponsesProvider::default()
    };
    let network = tau_provider::OutboundNetworkPolicy::from_environment(Default::default(), None);
    let mut bytes = Vec::new();
    let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
    let outcome = run_prompt_attempt(
        &prompt.agent_prompt_id,
        prompt,
        &provider,
        model,
        false,
        &mut writer,
        &mut || false,
        &network,
        tau_proto::ProviderAttempt::ONE,
    );
    server.join().expect("loopback server");
    outcome
}

/// A successful retried public Responses attempt retains the scheduler's
/// logical attempt ordinal in the terminal report.
#[test]
fn retried_attempt_retains_provider_attempt() {
    let prompt = crate::openai_tests::prompt();
    let provider = ResponsesProvider {
        base_url: "https://api.openai.com/v1".to_owned(),
        ..ResponsesProvider::default()
    };
    let attempt = tau_proto::ProviderAttempt::new(3).expect("provider attempt");
    let response = finished(
        &prompt.agent_prompt_id,
        &prompt,
        &provider,
        Vec::new(),
        tau_proto::ProviderStopReason::EndTurn,
        None,
        None,
        None,
        None,
        true,
        attempt,
    );
    assert_eq!(response.provider_attempt, attempt);
}

/// Generic Responses local summaries expose byte/timing progress without
/// exposing assistant or reasoning output before terminal validation.
#[test]
fn standalone_summary_progress_is_stats_only() {
    let mut prompt = crate::openai_tests::prompt();
    prompt.operation = tau_proto::PromptOperation::StandaloneCompaction;
    let mut sampler = ResponsesResponseSampler::new();
    sampler.latest_items = vec![
        tau_provider_responses::AttemptOutputItem {
            output_index: 0,
            display_generation: Default::default(),
            item: tau_proto::ContextItem::Message(tau_proto::MessageItem {
                role: tau_proto::ContextRole::Assistant,
                content: vec![tau_proto::ContentPart::Text {
                    text: "private narrative".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            }),
        },
        tau_provider_responses::AttemptOutputItem {
            output_index: 1,
            display_generation: Default::default(),
            item: tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "private reasoning".to_owned(),
            }),
        },
    ];
    sampler.latest_bytes = 42;
    let mut bytes = Vec::new();
    let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
    sampler.emit_at(
        &prompt.agent_prompt_id,
        &prompt,
        &mut writer,
        path_std_time::Instant::now(),
        true,
    );

    let mut reader =
        tau_proto::HarnessInputReader::new(path_std_io::BufReader::new(bytes.as_slice()));
    let message = reader
        .read_message()
        .expect("decode response update")
        .expect("one response update");
    let tau_proto::HarnessInputMessage::Emit(emit) = message else {
        panic!("sampler must emit a transient event");
    };
    let tau_proto::Event::ProviderResponseUpdatedReported(update) = *emit.event else {
        panic!("sampler must emit a provider update report");
    };
    assert!(update.deltas.is_empty());
    assert_eq!(
        update
            .response_stats
            .expect("content-free stats")
            .current
            .response_bytes_received,
        42
    );
}

/// Public Responses excludes arbitrary local lowering time and includes only
/// post-send semantic latency for both ordinary and standalone sampling.
#[test]
fn dispatch_reanchors_semantic_timing_without_changing_update_schema() {
    let attempt_started = path_std_time::Instant::now();
    let dispatched_at = attempt_started + path_std_time::Duration::from_secs(91);
    let semantic_at = dispatched_at + path_std_time::Duration::from_millis(17);

    for operation in [
        tau_proto::PromptOperation::Inference,
        tau_proto::PromptOperation::StandaloneCompaction,
    ] {
        let mut prompt = crate::openai_tests::prompt();
        prompt.operation = operation;
        let mut sampler = ResponsesResponseSampler::new_at(attempt_started);
        sampler.mark_dispatched(dispatched_at);
        sampler.observe_progress_at(semantic_at, true);
        sampler.latest_bytes = 1;

        let mut bytes = Vec::new();
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        sampler.emit_at(
            &prompt.agent_prompt_id,
            &prompt,
            &mut writer,
            semantic_at,
            true,
        );

        let mut reader =
            tau_proto::HarnessInputReader::new(path_std_io::BufReader::new(bytes.as_slice()));
        let message = reader
            .read_message()
            .expect("decode response update")
            .expect("one response update");
        let tau_proto::HarnessInputMessage::Emit(emit) = message else {
            panic!("sampler must emit a transient event");
        };
        let tau_proto::Event::ProviderResponseUpdatedReported(update) = *emit.event else {
            panic!("sampler must retain the existing response-update event");
        };
        let stats = update.response_stats.expect("existing stats payload");
        assert_eq!(stats.current.elapsed_micros, 17_000);
        assert_eq!(stats.first_semantic_output_elapsed_micros, Some(17_000));
    }
}

fn summary_config() -> SummaryCompactionConfig {
    SummaryCompactionConfig::new(
        NonZeroU64::new(128_000).expect("positive"),
        NonZeroU64::new(16_384).expect("positive"),
        NonZeroU32::new(4_096).expect("positive"),
        NonZeroU64::new(8_192).expect("positive"),
    )
    .expect("valid explicit summary profile")
}

/// Public Responses must share the generic fallback and fieldwise override
/// semantics without publishing a proactive threshold.
#[test]
fn local_summary_compaction_uses_generic_defaults_and_partial_overrides() {
    let provider: ResponsesProvider = serde_json::from_value(serde_json::json!({
        "models": [
            {"id": "default", "context_window": 8192},
            {
                "id": "partial",
                "context_window": 8192,
                "local_summary_compaction": {"max_output_tokens": 2048}
            }
        ]
    }))
    .expect("Responses models");
    provider
        .validate_local_summary_compaction()
        .expect("valid partial override");

    let published = models_for_provider(&tau_proto::ProviderName::new("responses"), &provider);
    assert!(
        published
            .iter()
            .all(|model| model.supports_standalone_compaction)
    );
    assert!(
        published
            .iter()
            .all(|model| model.standalone_compaction_threshold.is_none())
    );
    assert!(
        published
            .iter()
            .all(|model| model.standalone_compaction_prefix_budget.is_none())
    );

    let defaults = resolved_summary_config(&provider.models[0]).expect("generic fallback");
    assert_eq!(defaults.max_output_tokens(), 1024);
    let partial = resolved_summary_config(&provider.models[1]).expect("partial override");
    assert_eq!(partial.max_output_tokens(), 2048);
    assert_eq!(
        partial.max_output_bytes(),
        tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES as u64
    );
}

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
            context_window: tau_proto::TokenCount::new(128_000),
            max_input_tokens: None,
            max_output_tokens: None,
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
        tau_proto::ReasoningEffortCapability::mapped([
            tau_proto::NativeReasoningEffort::None,
            tau_proto::NativeReasoningEffort::Minimal,
            tau_proto::NativeReasoningEffort::Low,
            tau_proto::NativeReasoningEffort::Medium,
            tau_proto::NativeReasoningEffort::High,
            tau_proto::NativeReasoningEffort::XHigh,
            tau_proto::NativeReasoningEffort::Max,
        ])
    );
    assert!(!models[0].supports_compaction);
    assert!(models[0].supports_standalone_compaction);
    assert_eq!(models[0].standalone_compaction_threshold, None);
}

/// Context-window token typing must retain the Responses profile bytes, zero
/// and default behavior, published metadata, and scalar summary-backend
/// derivation.
#[test]
fn context_window_token_count_preserves_profile_and_summary_behavior() {
    let configured: ResponsesModel =
        serde_json::from_str(r#"{"id":"local","context_window":8192}"#).expect("configured model");
    assert_eq!(configured.context_window, tau_proto::TokenCount::new(8192));
    assert_eq!(
        serde_json::to_string(&configured).expect("configured model JSON"),
        r#"{"id":"local","context_window":8192}"#
    );
    assert_eq!(
        models_for_provider(
            &ProviderName::new("local"),
            &ResponsesProvider {
                models: vec![configured],
                ..ResponsesProvider::default()
            },
        )[0]
        .context_window,
        tau_proto::TokenCount::new(8192)
    );
    assert_eq!(
        resolved_summary_config(&ResponsesModel {
            id: ModelName::new("local"),
            efforts: None,
            compat: None,
            display_name: None,
            context_window: tau_proto::TokenCount::new(8192),
            max_input_tokens: None,
            max_output_tokens: None,
            tags: Vec::new(),
            supports_parallel_tool_calls: true,
            local_summary_compaction: None,
            cache_contract: None,
            est_uncached_input_cost_1m_usd: None,
            est_cached_input_cost_1m_usd: None,
            est_cache_write_input_cost_1m_usd: None,
            est_output_cost_1m_usd: None,
            est_cache_storage_cost_1m_token_hour_usd: None,
        })
        .expect("positive context window")
        .max_output_tokens(),
        1024
    );

    let defaulted: ResponsesModel =
        serde_json::from_str(r#"{"id":"defaulted"}"#).expect("defaulted model");
    assert_eq!(
        defaulted.context_window,
        tau_proto::TokenCount::new(128_000)
    );
    let zero: ResponsesModel =
        serde_json::from_str(r#"{"id":"zero","context_window":0}"#).expect("zero model");
    assert_eq!(zero.context_window, tau_proto::TokenCount::ZERO);
    assert!(
        resolved_summary_config(&zero).is_none(),
        "zero retains the prior disabled generic summary fallback"
    );
}

/// Public Responses profiles keep total, legal-input, model-output, and
/// provider policy limits distinct while legacy profiles retain old defaults.
#[test]
fn model_token_limits_are_independent_from_provider_output_policy() {
    let zero = serde_json::from_str::<ResponsesModel>(
        r#"{"id":"zero","context_window":8192,"max_output_tokens":0}"#,
    )
    .expect_err("zero output capability must not collide with omit-policy sentinel");
    assert!(zero.to_string().contains("nonzero"));
    let model: ResponsesModel = serde_json::from_str(
        r#"{"id":"wide","context_window":1050000,"max_input_tokens":922000,"max_output_tokens":128000}"#,
    )
    .expect("configured model limits");
    assert_eq!(model.requested_output_tokens(200_000), 128_000);
    assert_eq!(model.requested_output_tokens(8_192), 8_192);
    assert_eq!(model.requested_output_tokens(0), 0);
    let published = models_for_provider(
        &ProviderName::new("responses"),
        &ResponsesProvider {
            models: vec![model],
            max_output_tokens: 200_000,
            ..ResponsesProvider::default()
        },
    );
    assert_eq!(
        published[0].context_window,
        tau_proto::TokenCount::new(1_050_000)
    );
    assert_eq!(
        published[0].max_input_tokens,
        Some(tau_proto::TokenCount::new(922_000))
    );
    assert_eq!(
        published[0].max_output_tokens,
        Some(tau_proto::TokenCount::new(128_000))
    );
    assert_eq!(
        published[0].input_token_limit(),
        tau_proto::TokenCount::new(922_000)
    );
    let summary_limited: ResponsesModel =
        serde_json::from_str(r#"{"id":"summary","context_window":8192,"max_output_tokens":64}"#)
            .expect("summary-limited model");
    assert_eq!(
        resolved_summary_config(&summary_limited)
            .expect("summary support")
            .max_output_tokens(),
        64
    );
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
        tau_proto::ContextItem::Reasoning(
            tau_proto::OpaqueProviderItem::from_raw_json(
                r#"{"type":"reasoning","id":"reasoning-1"}"#,
            )
            .expect("valid reasoning item"),
        ),
        tau_proto::ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::Assistant,
            content: vec![tau_proto::ContentPart::Text {
                text: "useful checkpoint".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        }),
    ];
    let config = summary_config();
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
        hosted_tools: Vec::new(),
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
    let config = summary_config();
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
    let config = summary_config();
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
            "efforts": ["max", "low", "none", "xhigh"]
        }]
    }))
    .expect("profile");

    assert_eq!(
        models_for_provider(&tau_proto::ProviderName::new("responses"), &provider)[0].efforts,
        tau_proto::ReasoningEffortCapability::mapped([
            tau_proto::NativeReasoningEffort::None,
            tau_proto::NativeReasoningEffort::Low,
            tau_proto::NativeReasoningEffort::XHigh,
            tau_proto::NativeReasoningEffort::Max,
        ])
    );
    assert_eq!(
        provider.models[0]
            .efforts
            .as_ref()
            .expect("configured effort override")
            .as_slice(),
        &[
            tau_proto::NativeReasoningEffort::None,
            tau_proto::NativeReasoningEffort::Low,
            tau_proto::NativeReasoningEffort::XHigh,
            tau_proto::NativeReasoningEffort::Max,
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
        ResponsesNativeReasoningEfforts::try_from(vec![
            tau_proto::NativeReasoningEffort::Low,
            tau_proto::NativeReasoningEffort::Low
        ])
        .is_err()
    );
}

/// An explicit empty effort override must publish unsupported reasoning-effort
/// control so the provider omits a selector instead of inventing native none.
#[test]
fn profile_empty_responses_effort_override_disables_capability() {
    let provider: ResponsesProvider = serde_json::from_value(serde_json::json!({
        "models": [{"id": "example-model", "efforts": []}]
    }))
    .expect("profile");

    assert_eq!(
        models_for_provider(&tau_proto::ProviderName::new("responses"), &provider)[0].efforts,
        tau_proto::ReasoningEffortCapability::default()
    );
}

/// Public Responses plain reasoning progress must use the established full
/// reasoning delta so the existing `show-thinking` policy controls visibility.
#[test]
fn plain_reasoning_progress_emits_append_only_full_thinking() {
    let mut sampler = sampling::ResponsesResponseSampler::new();
    sampler.latest_items = vec![tau_provider_responses::AttemptOutputItem {
        output_index: 2,
        display_generation: Default::default(),
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
        display_generation: Default::default(),
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
        display_generation: Default::default(),
        item: tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text: "no".to_owned(),
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
        hosted_tools: Vec::new(),
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
            tau_proto::ProviderAttempt::ONE,
        );
    }
    assert_eq!(super::take_forwarded_debug_capture_policy(), [true, false]);
}
