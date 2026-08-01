//! Chat Completions extension ownership regression tests.

use std::{io as path_std_io, time as path_std_time};

use super::sampling::{RESPONSE_UPDATE_INTERVAL, ResponseSampler};
use super::*;

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
            supports_parallel_tool_calls: true,
            est_uncached_input_cost_1m_usd: None,
            est_cached_input_cost_1m_usd: None,
            est_output_cost_1m_usd: None,
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
            supports_parallel_tool_calls: true,
            est_uncached_input_cost_1m_usd: None,
            est_cached_input_cost_1m_usd: None,
            est_output_cost_1m_usd: None,
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
            supports_parallel_tool_calls: false,
            est_uncached_input_cost_1m_usd: Default::default(),
            est_cached_input_cost_1m_usd: Default::default(),
            est_output_cost_1m_usd: Default::default(),
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
