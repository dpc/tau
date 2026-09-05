//! Tests for prompt lifecycle behavior.

use super::super::lifecycle::{assert_no_message, connect_socket_ui, read_notice};
use super::*;
use crate::harness::prompt_materialization_timing::{diagnostic_work, reset_diagnostic_work};

#[derive(Clone)]
struct MaterializationTraceWriter(Arc<Mutex<Vec<u8>>>);

impl Write for MaterializationTraceWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("trace lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn register_sensitive_timing_fixture(h: &mut Harness) {
    h.tool_routing.registry.register(
        &crate::test_connection_id("provider-identifier-canary"),
        ToolSpec {
            name: ToolName::new("schema_canary_tool"),
            model_visible_name: None,
            description: Some("SCHEMA_DESCRIPTION_CANARY".to_owned()),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {"RAW_ARGS_CANARY": {"type": "string"}}
            })),
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
}

fn seed_sensitive_tool_context(h: &mut Harness, cid: &AgentId) {
    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(cid)
        .and_then(|agent| agent.identity.agent_id.clone())
        .expect("durable agent id");
    let prompt_id = test_agent_prompt_id("privacy-canary-prompt");
    let call_id: ToolCallId = "privacy-canary-call".into();
    let response = ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: prompt_id,
        agent_id: crate::parse_agent_id(&agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.clone(),
            name: ToolName::new("schema_canary_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Text("RAW_TOOL_ARGUMENT_CANARY".to_owned()),
            raw_arguments_json: Some(r#"{"secret":"RAW_JSON_ARGUMENT_CANARY"}"#.to_owned()),
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: Some("PROVIDER_RESPONSE_ID_CANARY".to_owned()),
        ws_pool_delta: None,
    };
    h.session_runtime
        .agent_store
        .append_agent_event(&agent_id, None, Event::ProviderResponseFinished(response))
        .expect("append sensitive tool call");
    let result = ToolResult {
        presentation: Default::default(),
        call_id,
        tool_name: ToolName::new("schema_canary_tool"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("TOOL_RESULT_CANARY".to_owned()),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: Arc::from(
                    [
                        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1,
                        0, 0, 0, 1, 8, 4, 0, 0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84,
                        120, 218, 99, 100, 248, 15, 0, 1, 5, 1, 1, 39, 24, 227, 102, 0, 0, 0, 0,
                        73, 69, 78, 68, 174, 66, 96, 130,
                    ]
                    .as_slice(),
                ),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,
        display: Some(tau_proto::ToolUseState {
            args: "IMAGE_PATH_METADATA_CANARY".to_owned(),
            ..Default::default()
        }),
    };
    let outcome = h
        .session_runtime
        .agent_store
        .append_agent_event(&agent_id, None, Event::ProviderToolResult(result))
        .expect("append sensitive tool result");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(cid)
        .expect("agent")
        .identity
        .head = outcome.selected_head_id.or(outcome.folded_node_id);
}

/// A real disabled dispatch performs no diagnostic clock, count, or schema
/// traversal work.
#[test]
fn disabled_provider_materialization_performs_zero_diagnostic_work() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    register_sensitive_timing_fixture(&mut h);
    seed_sensitive_tool_context(&mut h, &cid);
    reset_diagnostic_work();
    append_user_message_via_event(&mut h, "s1", "disabled privacy canary");
    let _ = h.send_prompt_to_agent("s1");
    assert_eq!(diagnostic_work(), 0);
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_materialization_timings
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// The disabled production path creates no pending timing owner, while an
/// enabled real prompt keeps sensitive transcript and identifier bytes out of
/// the fixed local trace.
#[test]
fn provider_materialization_trace_is_lazy_and_content_free_on_live_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    register_sensitive_timing_fixture(&mut h);
    seed_sensitive_tool_context(&mut h, &cid);
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_materialization_timings
            .is_empty()
    );

    let trace = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::clone(&trace);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .without_time()
        .with_ansi(false)
        .with_writer(move || MaterializationTraceWriter(Arc::clone(&writer)))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        append_user_message_via_event(&mut h, "s1", "PROMPT_CANARY SECRET_CANARY /PATH/CANARY");
        let _ = h.send_prompt_to_agent("s1");
    });

    let trace = String::from_utf8(trace.lock().expect("trace lock").clone()).expect("UTF-8 trace");
    assert!(trace.contains("tau_harness::prompt_materialization"));
    for canary in [
        "PROMPT_CANARY",
        "SECRET_CANARY",
        "/PATH/CANARY",
        "s1",
        "SCHEMA_DESCRIPTION_CANARY",
        "RAW_ARGS_CANARY",
        "provider-identifier-canary",
        "schema_canary_tool",
        "test/model",
        "RAW_TOOL_ARGUMENT_CANARY",
        "RAW_JSON_ARGUMENT_CANARY",
        "PROVIDER_RESPONSE_ID_CANARY",
        "TOOL_RESULT_CANARY",
        "IHDR",
        "IMAGE_PATH_METADATA_CANARY",
    ] {
        assert!(!trace.contains(canary), "leaked {canary}");
    }
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_materialization_timings
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// One provider dispatch sorts the live registry exactly once and reuses that
/// snapshot for tools, fragments, capabilities, and workdir filtering.
#[test]
fn provider_dispatch_reuses_one_sorted_tool_provider_snapshot() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let role = h.config.selected_role.clone();
    let model = h.config.selected_model.clone().expect("selected model");
    reset_dispatch_provider_sort_count();

    h.prepare_prompt_surface_for_dispatch(&role, None, None, &model, false, false)
        .expect("prepare dispatch surface");

    assert_eq!(dispatch_provider_sort_count(), 1);
    h.shutdown().expect("shutdown");
}

/// Manual benchmark reports dispatch-surface scaling while deterministic work
/// counters prove one provider sort per dispatch and parse-cache reuse after
/// warmup. It intentionally has no wall-clock pass/fail threshold.
#[test]
#[ignore = "manual prompt-surface dispatch scaling benchmark"]
fn benchmark_prompt_surface_dispatch_scaling() {
    let mut warm_source_parse_count = None;
    for dispatches in [1_usize, 10, 100] {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path().join("state")).expect("start");
        let role = h.config.selected_role.clone();
        let model = h.config.selected_model.clone().expect("selected model");
        reset_dispatch_provider_sort_count();
        reset_prompt_template_test_counters();
        let started = path_std_time::Instant::now();
        for _ in 0..dispatches {
            h.prepare_prompt_surface_for_dispatch(&role, None, None, &model, false, false)
                .expect("prepare dispatch surface");
        }
        eprintln!(
            "scenario=warm_inference text_context=default tool_surface=echo image_items=0 fanout=provider_plus_observers dispatches={dispatches} elapsed={:?} provider_sorts={} template_parses={} template_renders={}",
            started.elapsed(),
            dispatch_provider_sort_count(),
            prompt_template_parse_count(),
            prompt_template_render_count(),
        );
        assert_eq!(dispatch_provider_sort_count(), dispatches);
        let parses = prompt_template_parse_count();
        assert!(parses > 0);
        assert_eq!(*warm_source_parse_count.get_or_insert(parses), parses);
        h.shutdown().expect("shutdown");
    }
}

/// Provider-owned prompt fanout must preserve exact/prefix observer equality,
/// delivery metadata, provider exclusion, and typed-image projection semantics.
#[test]
fn provider_model_prompt_routes_directly_to_provider_owner() {
    // Provider-published models should not wake every provider subscriber.
    // The committed prompt remains visible to observers, while the owner gets a
    // direct delivery even without subscribing to agent.prompt_created.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let provider_frames = connect_ready_configured_extension(
        &mut h,
        "provider-owner",
        "provider-owner",
        tau_proto::ClientKind::Provider,
    );
    let provider_observer_frames =
        connect_test_client(&mut h, "provider-observer", tau_proto::ClientKind::Provider);
    let ui_frames = connect_test_client(&mut h, "ui-observer", tau_proto::ClientKind::Ui);
    let prefix_ui_frames =
        connect_test_client(&mut h, "prefix-ui-observer", tau_proto::ClientKind::Ui);
    let prompt_selector = vec![EventSelector::Exact(
        tau_proto::EventName::AGENT_PROMPT_CREATED,
    )];
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("provider-observer"),
            Vec::new(),
            prompt_selector.clone(),
        )
        .expect("provider observer subscription");
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("ui-observer"),
            Vec::new(),
            prompt_selector,
        )
        .expect("ui observer subscription");
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("prefix-ui-observer"),
            Vec::new(),
            vec![EventSelector::Prefix("agent.".to_owned())],
        )
        .expect("prefix UI observer subscription");

    let model_id: tau_proto::ModelId = "openai/gpt-5.5".parse().expect("model id");
    h.handle_extension_event(
        "provider-owner",
        TestProtocolItem::Event(Event::ProviderModelsDeclared(
            tau_proto::ProviderModelsDeclared {
                models: vec![tau_proto::ProviderModelInfo {
                    id: model_id.clone(),
                    display_name: None,
                    tags: Vec::new(),
                    hosted_tool_capabilities: Vec::new(),
                    supported_tool_types: vec![tau_proto::ToolType::Function],
                    input_modalities: Vec::new(),
                    tool_result_modalities: Vec::new(),
                    supports_parallel_tool_calls: true,
                    default_affinity: 0,
                    context_window: tau_proto::TokenCount::new(200_000),
                    max_input_tokens: None,
                    max_output_tokens: None,
                    efforts: tau_proto::ReasoningEffortCapability::mapped(vec![
                        tau_proto::NativeReasoningEffort::Medium,
                    ]),
                    verbosities: vec![tau_proto::Verbosity::Medium],
                    thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
                    supports_compaction: false,
                    supports_standalone_compaction: false,
                    standalone_compaction_generation_negative: false,
                    standalone_compaction_threshold: None,
                    standalone_compaction_prefix_budget: None,
                    cache_policy: None,
                    est_uncached_input_cost_1m_usd:
                        tau_proto::EstimatedUsdPerMillion::checked_from_usd(1),
                    est_cached_input_cost_1m_usd:
                        tau_proto::EstimatedUsdPerMillion::checked_from_usd(1),
                    est_cache_write_input_cost_1m_usd: None,
                    est_output_cost_1m_usd: tau_proto::EstimatedUsdPerMillion::checked_from_usd(1),
                    est_cache_storage_cost_1m_token_hour_usd: None,
                }],
            },
        )),
    )
    .expect("provider model snapshot");
    let mut earlier_duplicate = h.provider_runtime.models_by_extension["provider-owner"][0].clone();
    earlier_duplicate.est_uncached_input_cost_1m_usd =
        tau_proto::EstimatedUsdPerMillion::checked_from_usd(10);
    earlier_duplicate.est_cached_input_cost_1m_usd =
        tau_proto::EstimatedUsdPerMillion::checked_from_usd(10);
    earlier_duplicate.est_output_cost_1m_usd =
        tau_proto::EstimatedUsdPerMillion::checked_from_usd(10);
    h.provider_runtime
        .models_by_extension
        .get_mut("provider-owner")
        .expect("provider snapshot")
        .insert(0, earlier_duplicate);
    h.provider_runtime.model_info.insert(
        model_id.clone(),
        tau_proto::ProviderModelInfo {
            id: model_id.clone(),
            display_name: None,
            tags: Vec::new(),
            hosted_tool_capabilities: Vec::new(),
            supported_tool_types: vec![tau_proto::ToolType::Function],
            input_modalities: Vec::new(),
            tool_result_modalities: Vec::new(),
            supports_parallel_tool_calls: true,
            default_affinity: 0,
            context_window: tau_proto::TokenCount::new(200_000),
            max_input_tokens: None,
            max_output_tokens: None,
            efforts: tau_proto::ReasoningEffortCapability::mapped(vec![
                tau_proto::NativeReasoningEffort::Medium,
            ]),
            verbosities: vec![tau_proto::Verbosity::Medium],
            thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_generation_negative: false,
            standalone_compaction_threshold: None,
            standalone_compaction_prefix_budget: None,
            cache_policy: None,
            est_uncached_input_cost_1m_usd: tau_proto::EstimatedUsdPerMillion::checked_from_usd(10),
            est_cached_input_cost_1m_usd: tau_proto::EstimatedUsdPerMillion::checked_from_usd(10),
            est_cache_write_input_cost_1m_usd: None,
            est_output_cost_1m_usd: tau_proto::EstimatedUsdPerMillion::checked_from_usd(10),
            est_cache_storage_cost_1m_token_hour_usd: None,
        },
    );
    h.provider_runtime.model_routes.insert(
        model_id.clone(),
        crate::test_connection_id("provider-owner"),
    );
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role")
        .model = Some(model_id.clone());
    h.config.selected_model = Some(model_id);

    append_user_message_via_event(&mut h, "s1", "hello");
    let spid = h.send_prompt_to_agent("s1");
    let cid = h.prompt_coordination.prompt_runtime.agents[&spid].clone();

    let frame_is_prompt = |routed: &RoutedFrame, spid: &AgentPromptId| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::AgentPromptCreated(prompt))
                if prompt.agent_prompt_id.as_str() == spid.as_str()
        )
    };
    assert!(
        provider_frames
            .lock()
            .expect("provider frames")
            .iter()
            .any(|routed| frame_is_prompt(routed, &spid)),
        "provider owner should receive the direct prompt request"
    );
    assert!(
        ui_frames
            .lock()
            .expect("ui frames")
            .iter()
            .any(|routed| frame_is_prompt(routed, &spid)),
        "UI observer should still see the committed prompt fact"
    );
    assert!(
        prefix_ui_frames
            .lock()
            .expect("prefix UI frames")
            .iter()
            .any(|routed| frame_is_prompt(routed, &spid)),
        "prefix UI observer should see the same committed prompt fact"
    );
    assert!(
        provider_observer_frames
            .lock()
            .expect("provider observer frames")
            .is_empty(),
        "provider observers should not receive provider-owned prompt execution"
    );
    let prompt_frame = |frames: &Arc<Mutex<Vec<RoutedFrame>>>| {
        frames
            .lock()
            .expect("prompt frames")
            .iter()
            .find(|routed| frame_is_prompt(routed, &spid))
            .cloned()
            .expect("matching prompt frame")
    };
    let provider_frame = prompt_frame(&provider_frames);
    let exact_observer_frame = prompt_frame(&ui_frames);
    let prefix_observer_frame = prompt_frame(&prefix_ui_frames);
    assert_eq!(
        provider_frame
            .frame
            .as_delivery()
            .map(|delivery| (delivery.replay, delivery.recorded_at)),
        exact_observer_frame
            .frame
            .as_delivery()
            .map(|delivery| (delivery.replay, delivery.recorded_at)),
        "observer projection must preserve canonical delivery metadata"
    );
    assert_eq!(exact_observer_frame, prefix_observer_frame);
    let Event::AgentPromptCreated(provider_prompt) = provider_frame
        .frame
        .delivered_event()
        .expect("provider prompt event")
    else {
        unreachable!("selected frame is a prompt")
    };
    let Event::AgentPromptCreated(observer_prompt) = exact_observer_frame
        .frame
        .delivered_event()
        .expect("observer prompt event")
    else {
        unreachable!("selected frame is a prompt")
    };
    let mut expected_observer_prompt = provider_prompt.clone();
    expected_observer_prompt
        .context
        .clear_provider_image_bytes();
    assert_eq!(
        observer_prompt, &expected_observer_prompt,
        "observer prompt must differ only by stripped provider image bytes"
    );
    let prompt_image_bytes = |prompt: &tau_proto::AgentPromptCreated| {
        prompt.context.flatten_iter().find_map(|item| {
            let ContextItem::ToolResult(result) = item else {
                return None;
            };
            let [tau_proto::ToolResultContentPart::Image(image)] =
                result.provider_content.as_slice()
            else {
                return None;
            };
            Some(image.data.to_vec())
        })
    };
    let image_bytes = vec![1, 2, 3, 4];
    let mut typed_provider_prompt = provider_prompt.clone();
    typed_provider_prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![tau_proto::ToolResultItem {
                    presentation: Default::default(),
                    call_id: "call-observer-image".into(),
                    tool_type: tau_proto::ToolType::Function,
                    status: tau_proto::ToolResultStatus::Success,
                    output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("image".into())),
                    provider_content: vec![tau_proto::ToolResultContentPart::Image(
                        tau_proto::ImageContent {
                            media_type: tau_proto::ImageMediaType::Png,
                            data: image_bytes.clone().into(),
                            width: 1,
                            height: 1,
                            detail: tau_proto::ImageDetail::High,
                        },
                    )],
                }],
            },
        ));
    let Event::AgentPromptCreated(typed_observer_prompt) =
        crate::harness::event_without_provider_image_bytes(&Event::AgentPromptCreated(
            typed_provider_prompt.clone(),
        ))
    else {
        unreachable!("prompt projection preserves event identity")
    };
    assert_eq!(
        prompt_image_bytes(&typed_provider_prompt),
        Some(image_bytes)
    );
    assert_eq!(prompt_image_bytes(&typed_observer_prompt), Some(Vec::new()));

    h.provider_runtime
        .models_by_extension
        .get_mut("provider-owner")
        .expect("serving provider snapshot")[0]
        .est_uncached_input_cost_1m_usd = tau_proto::EstimatedUsdPerMillion::checked_from_usd(10);
    h.provider_runtime
        .model_info
        .get_mut(&"openai/gpt-5.5".into())
        .expect("flattened model metadata")
        .est_uncached_input_cost_1m_usd = tau_proto::EstimatedUsdPerMillion::checked_from_usd(10);
    let mut response =
        provider_text_response(&spid, crate::parse_agent_id("main"), "priced response");
    response.usage = Some(tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000_000,
        ..tau_proto::ProviderTokenUsage::default()
    });
    h.handle_extension_event(
        "provider-owner",
        TestProtocolItem::Event(Event::ProviderResponseFinishedReported(response)),
    )
    .expect("serving provider terminal");
    assert_eq!(
        h.agent_stats_snapshot(&cid)
            .expect("agent stats")
            .estimated_api_cost
            .as_picodollars(),
        1_000_000_000_000,
        "the successful provider route must retain its dispatch-time price"
    );

    h.shutdown().expect("shutdown");
    drop(h);
    wait_for_session_unlock(&sp, "s1");
    let mut resumed = echo_harness(&sp).expect("resume");
    let resumed_cid = ensure_test_user_agent(&mut resumed);
    assert_eq!(
        resumed
            .agent_stats_snapshot(&resumed_cid)
            .expect("resumed stats")
            .estimated_api_cost,
        tau_proto::EstimatedApiCost::default(),
        "runtime-only estimates must reset after cold reload"
    );
    resumed.shutdown().expect("resumed shutdown");
}

#[test]
fn provider_execution_events_must_come_from_prompt_owner() {
    // Provider execution is point-to-point. Once the harness routes a prompt to
    // the provider that published the selected model, streaming and final
    // response events for that prompt must come back from the same connection.
    // Otherwise a second provider participant could spoof a response for an
    // in-flight prompt it never received.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let _owner_frames = connect_ready_configured_extension(
        &mut h,
        "provider-owner",
        "provider-owner",
        tau_proto::ClientKind::Provider,
    );
    let _other_frames = connect_ready_configured_extension(
        &mut h,
        "provider-other",
        "provider-other",
        tau_proto::ClientKind::Provider,
    );
    let _tool_frames =
        connect_test_client(&mut h, "tool-impersonator", tau_proto::ClientKind::Tool);

    let model_id: tau_proto::ModelId = "openai/gpt-5.5".parse().expect("model id");
    h.handle_extension_event(
        "provider-owner",
        TestProtocolItem::Event(Event::ProviderModelsDeclared(
            tau_proto::ProviderModelsDeclared {
                models: vec![tau_proto::ProviderModelInfo {
                    id: model_id.clone(),
                    display_name: None,
                    tags: Vec::new(),
                    hosted_tool_capabilities: Vec::new(),
                    supported_tool_types: vec![tau_proto::ToolType::Function],
                    input_modalities: Vec::new(),
                    tool_result_modalities: Vec::new(),
                    supports_parallel_tool_calls: true,
                    default_affinity: 0,
                    context_window: tau_proto::TokenCount::new(200_000),
                    max_input_tokens: None,
                    max_output_tokens: None,
                    efforts: tau_proto::ReasoningEffortCapability::mapped(vec![
                        tau_proto::NativeReasoningEffort::Medium,
                    ]),
                    verbosities: vec![tau_proto::Verbosity::Medium],
                    thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
                    supports_compaction: false,
                    supports_standalone_compaction: false,
                    standalone_compaction_generation_negative: false,
                    standalone_compaction_threshold: None,
                    standalone_compaction_prefix_budget: None,
                    cache_policy: None,
                    est_uncached_input_cost_1m_usd: Default::default(),
                    est_cached_input_cost_1m_usd: Default::default(),
                    est_cache_write_input_cost_1m_usd: Default::default(),
                    est_output_cost_1m_usd: Default::default(),
                    est_cache_storage_cost_1m_token_hour_usd: None,
                }],
            },
        )),
    )
    .expect("provider model snapshot");
    h.provider_runtime.model_info.insert(
        model_id.clone(),
        tau_proto::ProviderModelInfo {
            id: model_id.clone(),
            display_name: None,
            tags: Vec::new(),
            hosted_tool_capabilities: Vec::new(),
            supported_tool_types: vec![tau_proto::ToolType::Function],
            input_modalities: Vec::new(),
            tool_result_modalities: Vec::new(),
            supports_parallel_tool_calls: true,
            default_affinity: 0,
            context_window: tau_proto::TokenCount::new(200_000),
            max_input_tokens: None,
            max_output_tokens: None,
            efforts: tau_proto::ReasoningEffortCapability::mapped(vec![
                tau_proto::NativeReasoningEffort::Medium,
            ]),
            verbosities: vec![tau_proto::Verbosity::Medium],
            thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_generation_negative: false,
            standalone_compaction_threshold: None,
            standalone_compaction_prefix_budget: None,
            cache_policy: None,
            est_uncached_input_cost_1m_usd: Default::default(),
            est_cached_input_cost_1m_usd: Default::default(),
            est_cache_write_input_cost_1m_usd: Default::default(),
            est_output_cost_1m_usd: Default::default(),
            est_cache_storage_cost_1m_token_hour_usd: None,
        },
    );
    h.provider_runtime.model_routes.insert(
        model_id.clone(),
        crate::test_connection_id("provider-owner"),
    );
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role")
        .model = Some(model_id.clone());
    h.config.selected_model = Some(model_id);

    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watcher_cid)
        .expect("watcher")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("watcher-busy"),
    };
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.dispatch_prompt_for_agent(&watched_cid, PendingPrompt::user("hello".to_owned()))
        .expect("dispatch watched prompt");
    let spid = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&watched_cid)
        .and_then(|agent| agent.dispatch.in_flight_prompt.clone())
        .expect("send watched prompt");
    assert_eq!(
        h.provider_runtime
            .pending_prompts
            .get(&spid)
            .map(|id| id.as_str()),
        Some("provider-owner"),
        "outbound prompt owner should be recorded"
    );

    h.handle_extension_event(
        "provider-other",
        TestProtocolItem::Event(Event::ProviderResponseUpdatedReported(
            ProviderResponseUpdated {
                agent_prompt_id: spid.clone(),
                agent_id: crate::parse_agent_id(&watched_id),
                deltas: Vec::new(),
                compaction: None,
                status: Some(tau_proto::ProviderResponseStatusUpdate {
                    text: "secret forged provider status".to_owned(),
                    clear_response: true,
                    retry: Some(tau_proto::ProviderRetryStatus {
                        category: tau_proto::ProviderRetryCategory::Transport,
                        attempt: 1,
                        next_retry_delay_secs: 2,
                    }),
                    native_tool: None,
                }),
                response_stats: None,
                originator: tau_proto::PromptOriginator::User,
            },
        )),
    )
    .expect("forged stream from provider");
    h.handle_extension_event(
        "tool-impersonator",
        TestProtocolItem::Event(Event::ProviderResponseUpdatedReported(
            ProviderResponseUpdated {
                agent_prompt_id: spid.clone(),
                agent_id: crate::parse_agent_id(&watched_id),
                deltas: Vec::new(),
                compaction: None,
                status: None,
                response_stats: None,
                originator: tau_proto::PromptOriginator::User,
            },
        )),
    )
    .expect("forged stream from tool");
    h.handle_extension_event(
        "provider-other",
        TestProtocolItem::Event(Event::ProviderResponseFinishedReported(
            provider_text_response(&spid, crate::parse_agent_id(&watched_id), "spoofed final"),
        )),
    )
    .expect("forged final response");

    assert_eq!(
        h.provider_runtime
            .pending_prompts
            .get(&spid)
            .map(|id| id.as_str()),
        Some("provider-owner"),
        "wrong-source events must not consume the pending owner"
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&watched_cid]
            .turn
            .turn_state,
        AgentTurnState::AgentThinking { .. }
    ));
    assert!(event_log_contains(&h, "provider-other", |event| matches!(
        event,
        Event::ProviderResponseUpdatedReported(_) | Event::ProviderResponseFinishedReported(_)
    )));
    assert!(!event_log_contains(
        &h,
        "tool-impersonator",
        |event| matches!(
            event,
            Event::ProviderResponseUpdatedReported(_) | Event::ProviderResponseFinishedReported(_)
        )
    ));
    assert!(
        session_agent_message_received_events(&h)
            .iter()
            .all(|message| message.kind != tau_proto::AgentMessageKind::WatchProviderStatus),
        "wrong-owner retry status must not reach watchers"
    );

    h.handle_extension_event(
        "provider-owner",
        TestProtocolItem::Event(Event::ProviderResponseUpdatedReported(
            ProviderResponseUpdated {
                agent_prompt_id: spid.clone(),
                agent_id: crate::parse_agent_id(&watched_id),
                deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
                    output_index: 0,
                    text: "real".to_owned(),
                    phase: None,
                }],
                compaction: None,
                status: Some(tau_proto::ProviderResponseStatusUpdate {
                    text: "secret raw owner diagnostic".to_owned(),
                    clear_response: true,
                    retry: Some(tau_proto::ProviderRetryStatus {
                        category: tau_proto::ProviderRetryCategory::Throttle,
                        attempt: 9,
                        next_retry_delay_secs: 10,
                    }),
                    native_tool: None,
                }),
                response_stats: None,
                originator: tau_proto::PromptOriginator::User,
            },
        )),
    )
    .expect("owner stream");
    let safe_status = session_agent_message_received_events(&h)
        .into_iter()
        .find(|message| message.kind == tau_proto::AgentMessageKind::WatchProviderStatus)
        .expect("owner retry status reaches watcher");
    assert!(safe_status.message.contains("throttle"));
    assert!(!safe_status.message.contains("secret raw owner diagnostic"));
    h.handle_extension_event(
        "provider-owner",
        TestProtocolItem::Event(Event::ProviderResponseFinishedReported(
            provider_text_response(&spid, crate::parse_agent_id(&watched_id), "real final"),
        )),
    )
    .expect("owner final response");

    assert!(!h.provider_runtime.pending_prompts.contains_key(&spid));
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&watched_cid]
            .turn
            .turn_state,
        AgentTurnState::Idle
    ));
    assert_eq!(
        agent_event_count(&h, |event| matches!(
            event,
            Event::ProviderResponseUpdated(_)
        )),
        0,
        "provider response updates must remain transient"
    );
    assert!(event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(event, Event::ProviderResponseFinished(_))
    ));

    h.shutdown().expect("shutdown");
}

#[test]
fn resume_keeps_prompt_appended_after_root_rewind_as_head() {
    // A durable root head move is only the cursor until a later transcript node
    // is appended. Resume must restore the replayed final tree head, not the
    // stale root rewind event.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let replacement_node: tau_core::NodeId;

    {
        let mut h = quiet_provider_harness(&sp).expect("start");
        append_user_message_via_event(&mut h, "s1", "first prompt");
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);

        h.handle_ui_navigate_tree(
            &crate::test_connection_id("ui"),
            tau_proto::UiNavigateTree {
                session_id: test_session_id("s1"),
                target_agent_id: Some(agent_id),
                target: tau_proto::UiTreeNavigationTarget::Root,
            },
        )
        .expect("navigate to root");
        append_user_message_via_event(&mut h, "s1", "replacement root prompt");
        replacement_node = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("replacement node");

        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let cid = ensure_test_user_agent(&mut h);
        let restored_head = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("restored selected head");
        assert!(
            default_agent_tree(&h)
                .branch_node_ids_from(Some(restored_head))
                .contains(&replacement_node),
            "shutdown fallback remains on the replacement branch"
        );

        append_user_message_via_event(&mut h, "s1", "continues after replacement");
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid].identity.head,
            Some(restored_head)
        );
        let checkpoint = h
            .session_runtime
            .agent_store
            .agent(
                h.agent_runtime.agent_registry.agents[&cid]
                    .identity
                    .agent_id
                    .as_deref()
                    .expect("agent id"),
            )
            .and_then(tau_core::AgentTree::unresolved_marked_inference_checkpoint)
            .cloned()
            .expect("restored owner");
        h.publish_for_agent(
            &cid,
            Event::AgentPromptTerminated(tau_proto::AgentPromptTerminated {
                automatic_compaction_decision: None,
                agent_id: checkpoint.agent_id,
                agent_prompt_id: checkpoint.agent_prompt_id,
                reason: tau_proto::AgentPromptTerminationReason::Stale,
                originator: tau_proto::PromptOriginator::User,
            }),
        );
        let continued = default_agent_tree(&h)
            .nodes()
            .last()
            .expect("continued prompt after resume");
        assert!(
            default_agent_tree(&h)
                .branch_node_ids_from(Some(continued.id))
                .contains(&replacement_node),
            "continued prompt remains on the replacement branch after ordered fallback drain"
        );

        h.shutdown().expect("shutdown");
    }
}

#[test]
fn resume_keeps_prompt_appended_after_anchor_rewind_as_head() {
    // A prompt-anchor rewind moves the cursor to the selected prompt's parent,
    // but a later replacement prompt must become the restored head after resume.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let replacement_node: tau_core::NodeId;

    {
        let mut h = quiet_provider_harness(&sp).expect("start");
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);

        append_user_message_via_event(&mut h, "s1", "first prompt");
        h.publish_for_agent(
            &cid,
            Event::ProviderResponseFinished(provider_text_response(
                &test_agent_prompt_id("sp-tree-resume-anchor"),
                agent_id.clone(),
                "assistant answer",
            )),
        );
        append_user_message_via_event(&mut h, "s1", "second prompt");

        h.handle_ui_navigate_tree(
            &crate::test_connection_id("ui"),
            tau_proto::UiNavigateTree {
                session_id: test_session_id("s1"),
                target_agent_id: Some(agent_id),
                target: tau_proto::UiTreeNavigationTarget::PromptAnchor(2),
            },
        )
        .expect("navigate before second prompt");
        append_user_message_via_event(&mut h, "s1", "replacement second prompt");
        replacement_node = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("replacement node");

        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let cid = ensure_test_user_agent(&mut h);
        let restored_head = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("restored selected head");
        assert!(
            default_agent_tree(&h)
                .branch_node_ids_from(Some(restored_head))
                .contains(&replacement_node),
            "shutdown fallback remains on the replacement branch"
        );

        append_user_message_via_event(&mut h, "s1", "continues after replacement");
        let continued = default_agent_tree(&h)
            .nodes()
            .last()
            .expect("continued prompt after resume");
        assert!(
            default_agent_tree(&h)
                .branch_node_ids_from(Some(continued.id))
                .contains(&replacement_node),
            "continued prompt remains on the replacement branch after ordered fallback drain"
        );

        h.shutdown().expect("shutdown");
    }
}

/// Tree formatting preserves exact root/anchor order, selection markers, and
/// prompt previews without exposing assistant/tool raw nodes.
#[test]
fn tree_request_result_formats_prompt_anchors_without_raw_nodes() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    append_user_message_via_event(&mut h, "s1", "first prompt");
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(provider_text_response(
            &test_agent_prompt_id("sp-tree-view"),
            agent_id.clone(),
            "assistant answer should not be listed",
        )),
    );
    append_user_message_via_event(&mut h, "s1", "second prompt");
    h.handle_ui_navigate_tree(
        &crate::test_connection_id("ui"),
        tau_proto::UiNavigateTree {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            target: tau_proto::UiTreeNavigationTarget::PromptAnchor(2),
        },
    )
    .expect("select second prompt anchor");

    let result = h.tree_request_result(&test_session_id("s1"), Some(agent_id.as_str()));
    assert_eq!(
        result,
        concat!(
            "    0   before first prompt (root)\n",
            "    1   before prompt  user: first prompt\n",
            "    2 * before prompt  user: second prompt",
        )
    );
}

/// When the agent reports a `response_id` on a finished turn, the
/// next `AgentPromptCreated` for that conversation must carry a
/// `previous_response_candidate` pointing back at it — that's the hook the
/// Responses backend uses to switch into stateful-chain mode and
/// send just the delta upstream. `next_item_index` must equal the
/// assembled item count at the moment the anchor was captured,
/// so the delta slice is exactly the items added since.
#[test]
fn response_id_anchors_next_prompt_with_previous_response() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    h.submit_user_prompt(test_session_id("s1"), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: Some(responses_backend()),
        provider_attempt: Default::default(),
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt(test_session_id("s1"), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );

    h.shutdown().expect("shutdown");
}

/// A skill loading mid-conversation (and surfacing into the system
/// prompt) must also bust the chain — the upstream stored its
/// reasoning state against the *previous* system prompt, and
/// chaining a request whose `instructions` field has new content
/// would silently mix the skill's guidance with reasoning that
/// never saw it. This is the more likely real-world trigger for a
/// fingerprint miss than a manual role-parameter flip: skills
/// auto-load as the agent works.
#[test]
fn system_prompt_drift_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    h.submit_user_prompt(test_session_id("s1"), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: Some("resp_skills".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // Simulate a skill becoming visible in the system prompt between
    // turns. `build_system_prompt` renders any `add_to_prompt: true`
    // skill into the prompt body, so inserting one here is the
    // narrowest way to make the system_prompt string drift without
    // touching unrelated state.
    h.prompt_coordination.context_discovery.skills.insert(
        tau_proto::SkillName::new("late-loaded"),
        crate::discovery::DiscoveredSkill {
            source_id: tau_proto::ConnectionId::parse("test-ext")
                .expect("test connection id must satisfy the identifier grammar"),
            description: "appears between turns".to_owned(),
            source: path_crate_discovery::DiscoveredSkillSource::File(
                path_std_path::PathBuf::from("/tmp/late-loaded.md"),
            ),
            add_to_prompt: true,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
            modified: None,
        },
    );

    h.submit_user_prompt(test_session_id("s1"), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );
}

#[test]
fn queued_prompt_extends_completed_first_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let first = h
        .submit_user_prompt(test_session_id("s1"), "first".to_owned())
        .expect("submit first");
    assert_eq!(first, PromptSubmission::Dispatched);
    let first_agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&test_user_agent(&h))
        .and_then(|conv| conv.identity.agent_id.clone())
        .expect("first prompt agent id");
    publish_pending_agent_discovery(&mut h, first_agent_id.as_str());
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();

    let second = h
        .submit_user_prompt(test_session_id("s1"), "second".to_owned())
        .expect("submit second");
    assert_eq!(second, PromptSubmission::Queued);

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish first");

    let prompt2 = read_nth_prompt_created(&h, 1);
    assert!(
        prompt1.context.flatten().len() < prompt2.context.flatten().len(),
        "queued follow-up should extend the first prompt"
    );
    assert_eq!(
        &prompt2.context.flatten()[..prompt1.context.flatten().len()],
        prompt1.context.flatten().as_slice()
    );
    let prompt2_items = prompt2.context.flatten();
    let last = prompt2_items.last().expect("last item");
    assert!(matches!(
        last,
        ContextItem::Message(MessageItem {
            role: ContextRole::User,
            ..
        })
    ));
    assert_eq!(text_part(last), Some("second"));

    h.shutdown().expect("shutdown");
}

/// Regression: a cold-resumed session needs one hidden restore notice in the
/// first provider prompt, but startup itself must not send that notice as a
/// standalone turn or as prewarm-only context.
#[test]
fn resumed_startup_folds_restore_notice_before_first_user_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let two_hours_ago = tau_proto::UnixMicros::new(
        tau_proto::UnixMicros::now()
            .get()
            .saturating_sub(2 * 60 * 60 * 1_000_000),
    );
    seed_prior_user_message_at(&sp, "before restore", two_hours_ago);

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");

    assert!(h.prompt_coordination.prompt_runtime.agents.is_empty());
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(_)
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptPrewarmRequested(prewarm)
            if prewarm
                .context.flatten()
                .iter()
                .any(|item| text_part(item).is_some_and(|text| crate::internal_envelope::body(text).is_some_and(is_restore_notice_prompt_text)))
    )));
    assert_eq!(restore_notice_event_count(&h), 0);

    h.submit_user_prompt(test_session_id("s1"), "after restore".to_owned())
        .expect("submit first resumed prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    let notice_pos = prompt
        .context
        .flatten()
        .iter()
        .position(|item| {
            text_part(item).is_some_and(|text| {
                crate::internal_envelope::body(text).is_some_and(is_restore_notice_prompt_text)
            })
        })
        .expect("restore notice in first prompt");
    let user_pos = prompt
        .context
        .flatten()
        .iter()
        .position(|item| text_part(item) == Some("after restore"))
        .expect("user prompt in first prompt");
    let notice = restore_notice_context_text(&prompt).expect("restore notice text");

    assert!(notice_pos < user_pos);
    assert!(notice.contains("Previous session was interrupted and restored."));
    assert!(notice.contains("2 hours have passed since the last recorded session event"));
    assert!(notice.contains("state of the world might have changed"));
    assert!(notice.contains("recreate timers"));
    assert_eq!(restore_notice_context_count(&prompt), 1);
    assert_eq!(restore_notice_event_count(&h), 1);

    h.shutdown().expect("shutdown");
}

/// Provider diagnostics must come from the provider that owns the prompt route.
#[test]
fn provider_cache_miss_diagnostic_requires_prompt_owner() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    connect_ready_configured_extension(
        &mut h,
        "provider-a",
        "provider-a",
        tau_proto::ClientKind::Provider,
    );
    connect_ready_configured_extension(
        &mut h,
        "provider-b",
        "provider-b",
        tau_proto::ClientKind::Provider,
    );
    h.provider_runtime.pending_prompts.insert(
        test_agent_prompt_id("prompt-1"),
        crate::test_connection_id("provider-a"),
    );

    let baseline_seq = h.runtime_io.event_log.next_seq();
    h.handle_extension_message(
        &crate::test_connection_id("provider-b"),
        TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ProviderCacheMissDiagnosticReported(
                cache_miss_diagnostic_for_test("prompt-1"),
            )),
            persist: true,
        }),
    )
    .expect("non-owner diagnostic emit");
    assert!(matches!(
        h.runtime_io
            .event_log
            .get_next_from(baseline_seq)
            .map(|entry| entry.event),
        Some(Event::ProviderCacheMissDiagnosticReported(_))
    ));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderCacheMissDiagnostic(_)
    )));

    h.handle_extension_message(
        &crate::test_connection_id("provider-a"),
        TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ProviderCacheMissDiagnosticReported(
                cache_miss_diagnostic_for_test("prompt-1"),
            )),
            persist: true,
        }),
    )
    .expect("owner diagnostic emit");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderCacheMissDiagnostic(diagnostic)
            if diagnostic.agent_prompt_id.as_str() == "prompt-1"
    )));

    h.shutdown().expect("shutdown");
}

/// Ensures full prompt rendering with AGENTS disabled returns only the system
/// wrapper and never falls back to harness-side filesystem discovery.
#[test]
fn rendered_prompt_without_agents_md_uses_system_wrapper_only() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    seed_render_prompt_role(&mut h);

    let result = request_rendered_prompt(&mut h, "debug-role", false);
    let prompt = result.prompt.expect("rendered prompt");

    assert_eq!(result.error, None);
    assert!(prompt.contains("<message role=\"system\">"));
    assert!(prompt.contains("DEBUG ROLE PROMPT"));
    assert!(!prompt.contains("Your agent id is"));
    assert!(!prompt.contains("source=\"AGENTS.md\""));
    assert!(!prompt.contains("AGENTS_FILE"));
    assert!(!prompt.contains("# agents.md files"));
}

/// Ensures full prompt rendering reports unknown roles in-band on the request
/// path instead of synthesizing a prompt.
#[test]
fn rendered_prompt_unknown_role_returns_in_band_error() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    seed_render_prompt_role(&mut h);

    let result = request_rendered_prompt(&mut h, "missing-role", true);

    assert_eq!(result.prompt, None);
    assert_eq!(result.error, Some("unknown role: missing-role".to_owned()));
}

/// A committed initial submission keeps its correlation until provider-prompt
/// materialization, so final shutdown emits a terminal instead of losing the
/// accepted prompt.
#[test]
fn initial_prompt_correlation_survives_submission_until_shutdown_terminal() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.complete_agent_publish(
        &cid,
        AgentPublishCompletion::InitialPromptSubmission {
            correlation: crate::agent::InitialPromptCorrelation {
                request_id: "create-shutdown".to_owned(),
                agent_id: agent_id.clone(),
                ctx_id: "prompt-shutdown".to_owned(),
                bootstrap_prompt: false,
                activation_through: None,
            },
        },
        tau_proto::AgentHead::Root,
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_initial_correlations
            .contains_key(&cid)
    );

    h.shutdown().expect("shutdown");
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptFailed(failed)
            if failed.request_id == "create-shutdown"
                && failed.agent_id == agent_id
                && failed.ctx_id == "prompt-shutdown"
                && failed.stage == tau_proto::AgentPromptFailureStage::LifecycleTeardown
    )));
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_initial_correlations
            .is_empty()
    );
}

/// A render/materialization error before `AgentPromptCreated` consumes the
/// retained correlation through `agent.prompt_failed`.
#[test]
fn initial_prompt_materialization_error_publishes_precreated_terminal() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.complete_agent_publish(
        &cid,
        AgentPublishCompletion::InitialPromptSubmission {
            correlation: crate::agent::InitialPromptCorrelation {
                request_id: "create-render".to_owned(),
                agent_id: agent_id.clone(),
                ctx_id: "prompt-render".to_owned(),
                bootstrap_prompt: false,
                activation_through: None,
            },
        },
        tau_proto::AgentHead::Root,
    );
    let prompt_id = tau_proto::AgentPromptId::parse("ap-render-failure").expect("prompt id");
    let mut failure = provider_text_response(&prompt_id, agent_id.clone(), "");
    failure.stop_reason = tau_proto::ProviderStopReason::Error;
    failure.error = Some("render failed".to_owned());
    h.publish_event_for_agent_with_completion(
        &cid,
        None,
        Event::ProviderResponseFinished(failure),
        None,
        false,
    );

    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptFailed(failed)
            if failed.request_id == "create-render"
                && failed.agent_id == agent_id
                && failed.ctx_id == "prompt-render"
                && failed.stage == tau_proto::AgentPromptFailureStage::Submission
    )));
}

/// A startup-blocked initial skill prompt must be preprocessed before a
/// message-wake steering drain; missing skills terminate instead of entering
/// the transcript as raw `:skill` text.
#[test]
fn queued_initial_skill_is_resolved_before_steering_drain() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    h.extensions.resolving_initial_collisions = true;
    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "create-steered-skill".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some(":skill missing-steered-skill".to_owned()),
            literal: false,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("prompt-steered-skill".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("queue initial prompt");
    let cid = test_user_agent(&h);
    assert!(!h.fold_pending_prompts_as_steered_with_completion(&cid, None));

    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptFailed(failed)
            if failed.request_id == "create-steered-skill"
                && failed.ctx_id == "prompt-steered-skill"
                && failed.stage == tau_proto::AgentPromptFailureStage::Preprocessing
    )));
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptSteered(prompt) if prompt.text == ":skill missing-steered-skill"
    )));
    h.extensions.resolving_initial_collisions = false;
    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::user("after steered missing skill".to_owned()),
    )
    .expect("dispatch later no-ctx prompt");
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.ctx_id.as_deref() == Some("prompt-steered-skill")
    )));
    assert!(read_nth_prompt_created(&h, 0).ctx_id.is_none());
}

/// Recalling an accepted queued initial prompt emits its correlated canceled
/// terminal before removing the queue entry.
#[test]
fn recalled_initial_prompt_publishes_correlated_terminal() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    h.extensions.resolving_initial_collisions = true;
    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "create-recalled".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some("queued prompt".to_owned()),
            literal: false,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("prompt-recalled".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("queue initial prompt");
    let cid = test_user_agent(&h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_recall_queued_prompt(&tau_proto::UiRecallQueuedPrompt {
        session_id: h.session_runtime.current_session_id.clone(),
        target_agent_id: Some(agent_id.clone()),
    });

    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptFailed(failed)
            if failed.request_id == "create-recalled"
                && failed.agent_id == agent_id
                && failed.ctx_id == "prompt-recalled"
                && failed.stage == tau_proto::AgentPromptFailureStage::Canceled
    )));
    h.extensions.resolving_initial_collisions = false;
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("after recall".to_owned()))
        .expect("dispatch later no-ctx prompt");
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptCreated(prompt) if prompt.ctx_id.as_deref() == Some("prompt-recalled")
    )));
    assert!(read_nth_prompt_created(&h, 0).ctx_id.is_none());
}

/// Cold resume retains the stable agent/session authority while a newly
/// materialized prompt supplies the exact model and effort for `self_info`.
#[test]
fn self_info_after_cold_resume_uses_resumed_identity_and_new_prompt_route() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let agent_id = {
        let mut h = echo_harness(&state).expect("start");
        let cid = ensure_test_user_agent(&mut h);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("seed".to_owned()))
            .expect("dispatch seed prompt");
        let prompt = read_nth_prompt_created(&h, 0);
        let agent_id = prompt.agent_id.clone();
        h.handle_provider_response_finished(provider_text_response(
            &prompt.agent_prompt_id,
            agent_id.clone(),
            "seed complete",
        ))
        .expect("finish seed prompt");
        h.shutdown().expect("shutdown seed harness");
        agent_id
    };

    let mut h = echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
        .expect("resume harness");
    let recorded = path_std_sync::Arc::new(RecordingSelfInfoTool(path_std_sync::Mutex::new(None)));
    h.install_internal_tool_handlers(vec![recorded.clone()]);
    let cid = h.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("inspect resumed self".to_owned()))
        .expect("dispatch resumed prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(provider_tool_response(
        &prompt,
        "resumed-self-info-call",
        "record_self_info",
        CborValue::Map(Vec::new()),
    ))
    .expect("run resumed self-info call");

    let info = recorded
        .0
        .lock()
        .expect("self-info recorder")
        .clone()
        .expect("recorded resumed metadata");
    assert_eq!(info.agent_id, agent_id);
    assert_eq!(info.session_id, test_session_id("s1"));
    assert_eq!(info.model, prompt.model);
    assert_eq!(info.effort, prompt.model_params.effort);
    h.shutdown().expect("shutdown resumed harness");
}

#[test]
fn recursive_delegate_prompt_contains_only_leaf_instruction() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    let default_cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = test_agent_prompt_id("sp-main");
    seed_agent_thinking(&mut h, &default_cid, "sp-main");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(main_spid.clone(), default_cid.clone());
    h.publish_for_agent(
        &default_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "ROOT: ask top delegate to delegate again".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "top-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-top".to_owned(),
            instruction: "TOP: delegate exactly two more subtasks".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("top-call".into()),
            task_name: Some("top".to_owned()),
        },
    )
    .expect("top query");

    let top_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("top prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: top_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "leaf-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("core-subagents"),
            query_id: "q-top".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("top response");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-leaf".to_owned(),
            instruction: "LEAF: do one terminal search only".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("leaf-call".into()),
            task_name: Some("leaf".to_owned()),
        },
    )
    .expect("leaf query");

    let leaf_spid = h
        .prompt_coordination.prompt_runtime.agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            matches!(
                h.agent_runtime.agent_registry.agents
                    .get(prompt_cid)
                    .map(|conv| &conv.identity.originator),
                Some(tau_proto::PromptOriginator::Extension { query_id, .. }) if query_id == "q-leaf"
            )
            .then_some(spid.clone())
        })
        .expect("leaf prompt id");
    let prompt = read_prompt_created(&h, &leaf_spid);
    let rendered = prompt
        .context
        .flatten()
        .iter()
        .filter_map(text_part)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        rendered.contains("LEAF: do one terminal search only"),
        "leaf prompt must include its own instruction; got: {rendered}",
    );
    assert!(
        !rendered.contains("TOP: delegate exactly two more subtasks"),
        "leaf prompt must not inherit parent recursive instruction; got: {rendered}",
    );
    assert!(
        !rendered.contains("ROOT: ask top delegate to delegate again"),
        "leaf prompt must not inherit ancestor task framing; got: {rendered}",
    );

    let tool_uses: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    assert!(
        tool_uses.is_empty(),
        "leaf prompt must not inherit unresolved ancestor tool calls; got: {tool_uses:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: one ordinary prompt's compact fact must enter the owning agent
/// store exactly once and advance its inference generation.
#[test]
fn ordinary_prompt_started_advances_persisted_inference_generation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("agent tree")
            .ordinary_inference_generation(),
        tau_proto::MaterializedPromptGeneration::from_inference_generation(0)
    );

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("ordinary inference".to_owned()))
        .expect("dispatch ordinary inference");
    let prompt_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .and_then(|agent| agent.dispatch.in_flight_prompt.clone())
        .expect("ordinary provider prompt");

    let prompt_records = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent events")
        .into_iter()
        .filter_map(|record| match record.event {
            Event::AgentPromptStarted(prompt) if prompt.agent_prompt_id == prompt_id => {
                Some(prompt)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompt_records.len(), 1);
    let prompt_record = &prompt_records[0];
    assert_eq!(
        prompt_record.operation,
        tau_proto::PromptOperation::Inference
    );
    assert!(prompt_record.model_params.is_some());
    let outer_turn_id = prompt_record
        .outer_turn_id
        .clone()
        .expect("ordinary prompt owns a durable outer turn");
    assert!(
        h.session_runtime
            .agent_store
            .agent_events(agent_id.as_str())
            .expect("agent events")
            .iter()
            .any(|record| matches!(
                &record.event,
                Event::AgentOuterTurnStarted(started)
                    if started.outer_turn_id == outer_turn_id
                    && matches!(
                        started.activation,
                        tau_proto::AgentOuterTurnActivation::Journal {
                            occurrence: tau_proto::AgentHead::Node(_)
                        }
                    )
            ))
    );
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("agent tree")
            .ordinary_inference_generation(),
        tau_proto::MaterializedPromptGeneration::from_inference_generation(1)
    );
    let full_prompt_count = event_log_events(&h)
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::AgentPromptCreated(prompt) if prompt.agent_prompt_id == prompt_id
            )
        })
        .count();
    assert!(
        h.send_prompt_to_agent_for(&cid).is_none(),
        "a folded compact fact must reject a second continuation before prompt construction"
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentPromptCreated(prompt) if prompt.agent_prompt_id == prompt_id
            ))
            .count(),
        full_prompt_count
    );

    h.handle_provider_response_finished(provider_text_response(
        &prompt_id,
        agent_id.clone(),
        "done",
    ))
    .expect("finish ordinary provider prompt");
    assert!(
        h.session_runtime
            .agent_store
            .agent_events(agent_id.as_str())
            .expect("agent events")
            .iter()
            .any(|record| matches!(
                &record.event,
                Event::AgentOuterTurnFinished(finished)
                    if finished.outer_turn_id == outer_turn_id
            ))
    );
    h.shutdown().expect("shutdown");
}

/// Regression: a user prompt can queue while another foreground tool from the
/// same provider turn is still running, before the model's later `wait` call is
/// dispatched. Starting that `wait` must notice the already-queued user input
/// and complete immediately instead of parking the agent behind background
/// work.
#[test]
fn wait_start_is_interrupted_by_already_queued_user_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _tool_events = connect_ready_configured_extension(
        &mut h,
        "conn-queued-wait",
        "configured-conn-queued-wait",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-queued-wait"),
        instant_background_test_tool_spec("slow_queued_wait"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let background_call_id: ToolCallId = "bg-queued-wait".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        background_call_id.as_str(),
        "slow_queued_wait",
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .pending_prompts
        .push_back(PendingPrompt::user("user input already queued".to_owned()));

    let wait_call_id: ToolCallId = "wait-queued-input".into();
    let wait_call = wait_no_args_call(wait_call_id.as_str());
    seed_tools_running(&mut h, &cid, vec![wait_call_id.clone()]);
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("wait interrupted by queued user input");

    assert_eq!(
        tool_result_count(&h, wait_call_id.as_str()),
        1,
        "wait should complete exactly once when queued user input preempts it"
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("wait_outcome: interrupted"))
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.text == "user input already queued"
    )));

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-queued-wait"),
        Event::ToolResultReported(final_tool_result(
            background_call_id.as_str(),
            "slow_queued_wait",
            "background done after interrupt",
        )),
    )
    .expect("background result after interrupted wait");
    assert_eq!(
        tool_result_count(&h, wait_call_id.as_str()),
        1,
        "the later background result must not resume a wait that never started"
    );

    h.shutdown().expect("shutdown");
}

/// An input wait is level-triggered: activating input accepted before the tool
/// call is handled completes it immediately without copying the queued payload
/// into the harness-authored result. This guards
/// `SPEC-tau-harness-activating-input-wait`.
#[test]
fn input_wait_returns_immediately_for_already_queued_activation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config
        .accepted_harness_settings
        .notification_delivery
        .user_prompt = NotificationDeliveryPolicy::from_millis(0, 1, 1)
        .expect("one-millisecond prospective wait policy");
    let cid = ensure_test_user_agent(&mut h);
    let call = wait_input_call("wait-input-queued");
    seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
    let durable_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    let submission = h
        .submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            &durable_id,
            PendingPrompt::human_ui("secret queued input".to_owned()),
        )
        .expect("queue activation");
    assert_eq!(submission, PromptSubmission::Queued);
    std::thread::sleep(Duration::from_millis(2));
    h.handle_wait_tool_call(&cid, &call, ToolName::new("wait"))
        .expect("input wait");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-input-queued"
                && result.result == CborValue::Map(vec![(
                    CborValue::Text("input_available".to_owned()),
                    CborValue::Bool(true),
                )])
    )));
    h.shutdown().expect("shutdown");
}

/// Registration-before-queue is the other half of the input-wait race: an
/// accepted internal extension-style prompt wakes exactly the addressed agent
/// while remaining queued for normal steering.
#[test]
fn input_wait_wakes_once_when_activating_prompt_is_queued() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let durable_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    let call = wait_input_call("wait-input-arrives");
    seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
    h.handle_wait_tool_call(&cid, &call, ToolName::new("wait"))
        .expect("register input wait");
    assert_eq!(tool_result_count(&h, call.id.as_str()), 0);
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));

    let submission = h
        .submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            &durable_id,
            PendingPrompt::internal("timer fired".to_owned()),
        )
        .expect("queue prompt");
    assert_eq!(submission, PromptSubmission::Queued);
    assert_eq!(tool_result_count(&h, call.id.as_str()), 1);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(prompt) if prompt.text == "timer fired"
    )));

    h.activate_waits_for(&cid, tau_proto::ObservationId::random());
    assert_eq!(tool_result_count(&h, call.id.as_str()), 1);
    h.shutdown().expect("shutdown");
}

/// A visible HumanUI prompt waits up to the approved five-second exact-wait
/// window, then interrupts once without consuming the awaited completion.
#[test]
fn human_ui_prompt_interrupts_exact_wait_at_five_second_deadline() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config
        .accepted_harness_settings
        .notification_delivery
        .user_prompt = HarnessSettings::built_in()
        .notification_delivery
        .user_prompt;
    let _tool_events = connect_test_tool(&mut h, "human-ui-deadline-tool");
    h.tool_routing.registry.register(
        &crate::test_connection_id("human-ui-deadline-tool"),
        instant_background_test_tool_spec("slow_human_ui_wait"),
    );
    let cid = ensure_test_user_agent(&mut h);
    let durable_id = durable_agent_id_for_conversation(&h, &cid);
    let background_call: ToolCallId = "human-ui-deadline-background".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        background_call.as_str(),
        "slow_human_ui_wait",
    );
    let wait_call = AgentToolCall {
        call_ref: None,
        id: "human-ui-deadline-wait".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(background_call.to_string()),
        )]),
    };
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("install exact wait");
    seed_tools_running(&mut h, &cid, vec![wait_call.id.clone()]);

    let before_admission = Instant::now();
    h.submit_prompt_to_agent(
        h.session_runtime.current_session_id.clone(),
        durable_id.as_str(),
        PendingPrompt::human_ui("urgent visible input".to_owned()),
    )
    .expect("queue HumanUI prompt");
    h.process_notification_delivery_deadlines_at(before_admission + Duration::from_millis(4_999));
    assert_eq!(tool_result_count(&h, wait_call.id.as_str()), 0);
    h.process_notification_delivery_deadlines_at(before_admission + Duration::from_millis(5_001));
    assert_eq!(tool_result_count(&h, wait_call.id.as_str()), 1);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id == wait_call.id
                && matches!(&result.result, CborValue::Text(text) if text.contains("wait_mode: exact"))
    )));
}

/// Successful preemption compacts the complete closed cancellation round, then
/// releases queued activating input against the replacement window.
#[test]
fn successful_wait_preemption_installs_replacement_before_queued_activation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let wait = wait_input_call("wait-preempt-success");
    seed_assistant_tool_round(&mut h, &cid, &[(wait.id.as_str(), "wait")]);
    seed_tools_running(&mut h, &cid, vec![wait.id.clone()]);
    h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
        .expect("install input wait");
    let (requesting_ui_id, mut requesting_ui) = connect_socket_ui(&mut h);
    let (_observer_id, mut observer) = connect_socket_ui(&mut h);
    let baseline_log_len = event_log_events(&h).len();
    h.handle_compact_request(
        &requesting_ui_id,
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    assert_eq!(read_notice(&mut requesting_ui).message, "compaction queued");
    let compact_prompt = read_nth_prompt_created(&h, 0);

    assert_eq!(
        h.submit_prompt_to_agent(
            test_session_id("s1"),
            agent_id.as_str(),
            PendingPrompt::internal("queued after compact".to_owned()),
        )
        .expect("queue activating input"),
        PromptSubmission::Queued
    );
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(event, Event::AgentPromptSteered(prompt)
            if prompt.text == "queued after compact")
    }));
    h.handle_provider_response_finished(standalone_compaction_success_response(
        &compact_prompt,
        "replacement summary",
    ))
    .expect("accept compaction");
    h.drain_publish_idle_dispatches();
    h.try_advance_queue();

    assert!(
        agent_tree_for_conversation(&h, &cid)
            .current_branch()
            .iter()
            .any(|entry| matches!(
                entry,
                tau_core::AgentEntry::Compaction {
                    replacement_window,
                    ..
                } if matches!(
                    replacement_window.as_slice(),
                    [ContextItem::Message(message)]
                        if matches!(
                            message.content.as_slice(),
                            [ContentPart::Text { text }] if text == "replacement summary"
                        )
                )
            ))
    );
    let continuation = read_nth_prompt_created(&h, 1);
    let continuation_text: Vec<_> = continuation
        .context
        .flatten()
        .iter()
        .filter_map(text_part)
        .map(str::to_owned)
        .collect();
    assert!(continuation_text.contains(&"replacement summary".to_owned()));
    assert!(continuation_text.contains(&crate::internal_envelope::frame("queued after compact"),));
    assert_no_message(&mut requesting_ui);
    assert_no_message(&mut observer);
    assert!(
        event_log_events(&h)[baseline_log_len..]
            .iter()
            .all(|event| !matches!(
                event,
                Event::HarnessNotice(notice)
                    if notice.purpose == tau_proto::NoticePurpose::Response
            ))
    );
}

/// If completion B is already queued, an exact wait for still-running A sees
/// that activating input regardless of queue/register order and is preempted;
/// a bare wait for B itself still consumes B by completion arbitration.
#[test]
fn queued_other_completion_preempts_exact_wait_but_remains_bare_waitable() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_a: ToolCallId = "background-a".into();
    let call_b: ToolCallId = "background-b".into();
    for call_id in [&call_a, &call_b] {
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert(call_id.clone(), cid.clone());
        h.tool_routing.tool_runtime.pending_tools.insert(
            call_id.clone(),
            PendingTool {
                name: ToolName::new("slow"),
                internal_name: ToolName::new("slow"),
                tool_type: tau_proto::ToolType::Function,
                allows_provider_image: false,
            },
        );
        h.record_wait_tool_request(call_id);
        h.record_wait_tool_result(
            &ToolResult {
                presentation: Default::default(),
                call_id: call_id.clone(),
                tool_name: ToolName::new("slow"),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text("running".to_owned()),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            },
            Some(tau_proto::ObservationId::random()),
        );
    }
    h.tool_routing
        .tool_runtime
        .background_completion_targets
        .insert(call_b.clone(), cid.clone());
    h.record_wait_background_result(
        tau_proto::ToolBackgroundResult {
            call_id: call_b.clone(),
            tool_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("B done".to_owned()),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        },
        Some(tau_proto::ObservationId::random()),
    );
    h.queue_background_completion_prompt_without_advancing(&cid, &call_b);

    let exact = AgentToolCall {
        call_ref: None,
        id: "wait-exact-a".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(call_a.to_string()),
        )]),
    };
    h.handle_wait_tool_call(&cid, &exact, ToolName::new("wait"))
        .expect("preempt exact A");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id == exact.id
                && matches!(&result.result, CborValue::Text(text)
                    if text.contains("wait_outcome: interrupted"))
    )));

    let bare = wait_no_args_call("wait-bare-b");
    h.handle_wait_tool_call(&cid, &bare, ToolName::new("wait"))
        .expect("consume B");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id == bare.id
                && cbor_map_text(&result.result, "original_tool_call_id")
                    == Some(call_b.as_str())
    )));
    h.shutdown().expect("shutdown");
}

/// A consumable completed candidate suppresses only its own queued notice for
/// start-time arbitration; a second completed call's notice still preempts both
/// exact and bare waits rather than being hidden by a coarse boolean.
#[test]
fn distinct_queued_completion_preempts_wait_with_consumable_candidate() {
    for exact in [true, false] {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path().join("state")).expect("start");
        let cid = ensure_test_user_agent(&mut h);
        let call_a: ToolCallId = format!("completed-a-{exact}").into();
        let call_b: ToolCallId = format!("completed-b-{exact}").into();
        for call_id in [&call_a, &call_b] {
            h.tool_routing
                .tool_runtime
                .background_completion_targets
                .insert(call_id.clone(), cid.clone());
            h.record_wait_background_result(
                tau_proto::ToolBackgroundResult {
                    call_id: call_id.clone(),
                    tool_name: ToolName::new("slow"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text(format!("{call_id} done")),
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                },
                Some(tau_proto::ObservationId::random()),
            );
            h.queue_background_completion_prompt_without_advancing(&cid, call_id);
        }
        let wait = if exact {
            AgentToolCall {
                call_ref: None,
                id: "wait-completed-exact-a".into(),
                name: ToolName::new("wait"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("tool_call_id".to_owned()),
                    CborValue::Text(call_a.to_string()),
                )]),
            }
        } else {
            wait_no_args_call("wait-completed-bare-a")
        };
        h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
            .expect("other completion preempts wait");
        assert!(event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::ToolResult(result)
                if result.call_id == wait.id
                    && matches!(&result.result, CborValue::Text(text)
                        if text.contains("wait_outcome: interrupted"))
        )));
        let consume_a = AgentToolCall {
            call_ref: None,
            id: format!("consume-a-{exact}").into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text(call_a.to_string()),
            )]),
        };
        h.handle_wait_tool_call(&cid, &consume_a, ToolName::new("wait"))
            .expect("A remains consumable");
        assert!(event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::ToolResult(result) if result.call_id == consume_a.id
        )));
        h.shutdown().expect("shutdown");
    }
}

/// Named context-size alerts fire as internal prompts only after their
/// thresholds are exceeded, remain one-shot while usage stays high, and become
/// eligible again after usage falls back below the threshold.
#[test]
fn named_context_size_alerts_queue_once_per_usage_crossing() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    role.context_size_alerts.insert(
        "compact-soon".to_owned(),
        tau_config::settings::ContextSizeAlert {
            threshold: path_tau_config_settings::ContextSizeAlertThreshold::new(100)
                .expect("positive test threshold"),
            enable: true,
            message: "compact soon".to_owned(),
            when: tau_config::settings::ContextPolicyWhen {
                at: path_tau_config_settings::ContextPolicyPoint::AfterResponse,
                statuses: None,
            },
        },
    );
    role.context_size_alerts.insert(
        "later".to_owned(),
        tau_config::settings::ContextSizeAlert {
            threshold: path_tau_config_settings::ContextSizeAlertThreshold::new(200)
                .expect("positive test threshold"),
            enable: true,
            message: "compact now".to_owned(),
            when: tau_config::settings::ContextPolicyWhen {
                at: path_tau_config_settings::ContextPolicyPoint::AfterResponse,
                statuses: None,
            },
        },
    );
    role.context_size_alerts.insert(
        "disabled".to_owned(),
        tau_config::settings::ContextSizeAlert {
            threshold: path_tau_config_settings::ContextSizeAlertThreshold::new(1)
                .expect("positive test threshold"),
            enable: false,
            message: "must not appear".to_owned(),
            when: tau_config::settings::ContextPolicyWhen {
                at: path_tau_config_settings::ContextPolicyPoint::AfterResponse,
                statuses: None,
            },
        },
    );
    let alerts = role.context_size_alerts.clone();

    h.queue_crossed_context_size_alerts(&cid, Some(100), &alerts);
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .is_empty()
    );

    h.queue_crossed_context_size_alerts(&cid, Some(250), &alerts);
    let prompts = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .pending_prompts
        .iter()
        .map(|prompt| (prompt.text.as_str(), prompt.message_class))
        .collect::<Vec<_>>();
    assert_eq!(
        prompts,
        vec![
            ("compact soon", tau_proto::PromptMessageClass::Internal),
            ("compact now", tau_proto::PromptMessageClass::Internal),
        ]
    );

    h.queue_crossed_context_size_alerts(&cid, Some(300), &alerts);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .len(),
        2
    );
    h.queue_crossed_context_size_alerts(&cid, Some(50), &alerts);
    h.queue_crossed_context_size_alerts(&cid, Some(250), &alerts);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .len(),
        4
    );
    h.clear_agent_context_usage(&cid);
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .is_empty()
    );
    h.queue_crossed_context_size_alerts(&cid, Some(250), &alerts);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .len(),
        2
    );

    h.shutdown().expect("shutdown");
}

/// Alert policy belongs to the dispatched prompt, so changing the interactive
/// role while the provider is running cannot substitute another role's message.
#[test]
fn context_size_alert_uses_prompt_owned_role_snapshot() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role")
        .context_size_alerts
        .insert(
            "compact-soon".to_owned(),
            tau_config::settings::ContextSizeAlert {
                threshold: path_tau_config_settings::ContextSizeAlertThreshold::new(100)
                    .expect("positive test threshold"),
                enable: true,
                message: "original role alert".to_owned(),
                when: tau_config::settings::ContextPolicyWhen {
                    at: path_tau_config_settings::ContextPolicyPoint::AfterResponse,
                    statuses: None,
                },
            },
        );
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("work".to_owned()))
        .expect("dispatch");
    let prompt = read_nth_prompt_created(&h, 0);

    let mut replacement_role = path_tau_config_settings::AgentRole::default();
    replacement_role.context_size_alerts.insert(
        "compact-soon".to_owned(),
        tau_config::settings::ContextSizeAlert {
            threshold: path_tau_config_settings::ContextSizeAlertThreshold::new(1)
                .expect("positive test threshold"),
            enable: true,
            message: "replacement role alert".to_owned(),
            when: tau_config::settings::ContextPolicyWhen {
                at: path_tau_config_settings::ContextPolicyPoint::AfterResponse,
                statuses: None,
            },
        },
    );
    h.config
        .available_roles
        .insert("replacement".to_owned(), replacement_role);
    h.config.selected_role = "replacement".to_owned();

    let mut response =
        provider_text_response(&prompt.agent_prompt_id, prompt.agent_id, "finished work");
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 101,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 2,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(response)
        .expect("finish response");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == "original role alert"
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == "replacement role alert"
    )));
    h.shutdown().expect("shutdown");
}

/// Explicit removal of a captured model decides one restored continuation:
/// the exact checkpoint commits, no provider prompt is sent, and watchers get a
/// sanitized categorical terminal state.
#[test]
fn restored_continuation_terminalizes_on_explicit_model_removal() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = crate::parse_agent_id(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .as_deref()
            .expect("durable agent"),
    );
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watcher_cid)
        .expect("watcher")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("ap-busy-removal-watcher"),
    };
    h.set_agent_watch(
        &watcher_id,
        agent_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let model: tau_proto::ModelId = "test/model".into();
    let provider = h.provider_runtime.model_routes[&model].to_string();
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-restored-removal").expect("transaction");
    let compact_prompt_id: tau_proto::AgentPromptId =
        test_agent_prompt_id("ap-restored-removal-compact");
    let checkpoint_prompt_id: tau_proto::AgentPromptId =
        test_agent_prompt_id("ap-restored-removal-inference");
    let started = tau_proto::AgentStandaloneCompactionStarted {
        agent_id: agent_id.clone(),
        transaction_id: transaction_id.clone(),
        compact_prompt_id: compact_prompt_id.clone(),
        cut: tau_proto::AgentHead::Root,
        resume_through: Some(tau_proto::AgentHead::Root),
        model: model.clone(),
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        originator: tau_proto::PromptOriginator::User,
        supersedes: None,
        trigger: tau_proto::StandaloneCompactionTrigger::Manual,
    };
    h.session_runtime
        .agent_store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::Root,
            Event::AgentStandaloneCompactionStarted(started.clone()),
            tau_proto::UnixMicros::now(),
        )
        .expect("seed start");
    h.session_runtime
        .agent_store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::Root,
            Event::AgentCompacted(tau_proto::AgentCompacted {
                original_input_tokens: None,
                compaction_output_tokens: None,
                agent_id: agent_id.clone(),
                transaction_id: Some(transaction_id.clone()),
                cut: Some(tau_proto::AgentHead::Root),
                suffix_end: Some(tau_proto::AgentHead::Root),
                compact_prompt_id: Some(compact_prompt_id),
                model: Some(model.clone()),
                operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
                replacement_window: vec![ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content: vec![ContentPart::Text {
                        text: "summary".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                })],
            }),
            tau_proto::UnixMicros::now(),
        )
        .expect("seed success");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .activation_dispatch = path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
        owner: path_crate_agent::InferenceCheckpointOwner::Standalone {
            id: transaction_id.clone(),
        },
        agent_prompt_id: checkpoint_prompt_id.clone(),
        through: tau_proto::AgentHead::Root,
        dispatch: crate::agent::InferenceDispatchOwnership {
            model: model.clone(),
            operation: tau_proto::PromptOperation::Inference,
            activation_cut: tau_proto::AgentHead::Root,
        },
    };
    let other_model: tau_proto::ModelId = "other/current".into();
    let mut other_info = h.provider_runtime.model_info[&model].clone();
    other_info.id = other_model.clone();
    h.provider_runtime
        .model_info
        .insert(other_model.clone(), other_info);
    h.provider_runtime.model_routes.insert(
        other_model.clone(),
        tau_proto::ConnectionId::parse("other-provider")
            .expect("test connection id must satisfy the identifier grammar"),
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .identity
        .model_override = Some(other_model);

    h.publish_provider_models_update(
        &crate::test_connection_id(&provider),
        crate::test_extension_name(provider.clone()),
        tau_proto::ProviderModelsDeclared { models: Vec::new() },
    );

    let events = event_log_events(&h);
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentInferenceDispatchStarted(checkpoint)
            if checkpoint.transaction_id.as_ref() == Some(&transaction_id)
                && checkpoint.agent_prompt_id == checkpoint_prompt_id
                && checkpoint.model.as_ref() == Some(&model)
                && checkpoint.operation == Some(tau_proto::PromptOperation::Inference)
                && checkpoint.activation_cut == Some(tau_proto::AgentHead::Root)
                && checkpoint.through == tau_proto::AgentHead::Root
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(prompt) if prompt.agent_prompt_id == checkpoint_prompt_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ProviderResponseFinished(response)
            if response.agent_prompt_id == checkpoint_prompt_id
                && response.failure_kind == Some(tau_proto::ProviderFailureKind::Unknown)
    )));
    assert!(matches!(
        h.agent_runtime.agent_watch.provider_status[agent_id.as_str()].state,
        tau_proto::AgentWatchProviderState::TerminalError { .. }
    ));
    assert!(
        session_agent_message_received_events(&h)
            .iter()
            .any(|message| {
                message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
                    && message.recipient_id.as_str() == watcher_id
                    && message
                        .watch_provider_status
                        .as_ref()
                        .is_some_and(|status| {
                            matches!(
                                status.state,
                                tau_proto::AgentWatchProviderState::TerminalError {
                                    failure_kind: tau_proto::ProviderFailureKind::Unknown,
                                    ..
                                }
                            )
                        })
            })
    );
}

/// Timer-origin internal prompts must retain their approved typed activation
/// classification without text or extension-name inference.
#[test]
fn timer_prompt_source_maps_to_timer_activation_observation() {
    let mut prompt = path_crate_agent::PendingPrompt::internal("wake".into());
    prompt.source = path_crate_agent::PendingPromptSource::Timer;
    assert_eq!(prompt.activation_kind(), tau_proto::ActivationKind::Timer);
}

/// Inference-driving prompts receive one durable activation identity before
/// submission; passive notices remain deliberately non-activating.
#[test]
fn prompt_activation_observation_is_allocated_once_and_skips_passive_notices() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = harness.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent id");

    let mut activating = PendingPrompt::internal("activate".into());
    harness.ensure_prompt_activation_observed(&cid, &mut activating);
    let activation = activating
        .activation_observation
        .expect("activation identity");
    harness.ensure_prompt_activation_observed(&cid, &mut activating);

    let records = harness
        .session_runtime
        .agent_store
        .agent_events(&agent_id)
        .expect("agent records");
    assert_eq!(
        records
            .iter()
            .filter(|record| {
                record.observation_id == activation
                    && matches!(
                        record.event,
                        Event::AgentActivationQueued(tau_proto::AgentActivationQueued {
                            kind: tau_proto::ActivationKind::InternalPrompt,
                            ..
                        })
                    )
            })
            .count(),
        1
    );
    let mut passive = PendingPrompt::passive_background_completion("passive".into());
    harness.ensure_prompt_activation_observed(&cid, &mut passive);
    assert!(passive.activation_observation.is_none());
}

/// Regression: startup has no implicit `main` agent. The first interactive
/// prompt claims the default conversation by minting a durable role-prefixed
/// hex agent id, publishes that id on `AgentPromptStarted`/`AgentPromptCreated`
/// for UI/provider routing, and omits the id from the provider-bound built-in
/// system prompt because `self_info` now owns model-visible discovery. It also
/// locks in that the lightweight lifecycle event immediately precedes the full
/// provider prompt.
#[test]
fn user_prompt_mints_first_agent_for_empty_startup() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let existing_agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .values()
        .find(|conversation| conversation.identity.originator.is_user())
        .and_then(|conversation| conversation.identity.agent_id.clone());

    h.submit_user_prompt(test_session_id("s1"), "hello".to_owned())
        .expect("submit first user prompt");

    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&test_user_agent(&h))
        .and_then(|conversation| conversation.identity.agent_id.as_deref())
        .expect("first prompt minted agent id");
    if let Some(existing_agent_id) = existing_agent_id {
        assert_eq!(agent_id, existing_agent_id.as_str());
    } else {
        assert_role_hex_agent_id(agent_id, "engineer");
    }
    assert_eq!(
        h.agent_runtime.agent_registry.agent_routes.get(agent_id),
        Some(&test_user_agent(&h))
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(prompt)
            if prompt.agent_id.as_str() == agent_id
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_id.as_str() == agent_id
    )));
    let prompt = read_nth_prompt_created(&h, 0);
    let events = event_log_events(&h);
    let prompt_pair = events
        .windows(2)
        .find_map(|window| match (&window[0], &window[1]) {
            (Event::AgentPromptStarted(started), Event::AgentPromptCreated(created))
                if started.agent_prompt_id == created.agent_prompt_id =>
            {
                Some((started, created))
            }
            _ => None,
        })
        .expect("prompt_started immediately precedes matching prompt_created");
    assert_eq!(prompt_pair.0.agent_id, prompt_pair.1.agent_id);
    assert_eq!(prompt_pair.0.session_id, prompt_pair.1.session_id);
    assert_eq!(prompt_pair.0.model, prompt_pair.1.model);
    assert_eq!(prompt_pair.0.originator, prompt_pair.1.originator);
    assert_eq!(prompt_pair.0.ctx_id, prompt_pair.1.ctx_id);
    assert_eq!(prompt.agent_id.as_str(), agent_id);
    assert!(!prompt.system_prompt.contains("# Agent identity"));
    assert!(!prompt.system_prompt.contains(agent_id));

    h.shutdown().expect("shutdown");
}

/// Regression: `agent_id` is a first-class system prompt template variable, not
/// a harness-level post-render prefix. A role `prompt_override` can therefore
/// place the id in custom wording even though built-in templates omit it.
#[test]
fn prompt_override_template_can_place_agent_id_without_default_duplication() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let selected_role = h.config.selected_role.clone();
    h.prompt_coordination
        .context_discovery
        .system_prompt_templates
        .insert(
            "custom-template".to_owned(),
            "Custom identity placement: {{agent_id}}\n\nRole={{role.name}}".to_owned(),
        );
    h.config
        .available_roles
        .entry(selected_role.clone())
        .or_default()
        .prompt_override = Some("custom-template".to_owned());

    h.submit_user_prompt(test_session_id("s1"), "hello".to_owned())
        .expect("submit first user prompt");

    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&test_user_agent(&h))
        .and_then(|conversation| conversation.identity.agent_id.as_deref())
        .expect("first prompt minted agent id");
    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(
        prompt.system_prompt,
        format!("Custom identity placement: {agent_id}\n\nRole={selected_role}")
    );
    assert_eq!(prompt.system_prompt.matches(agent_id).count(), 1);
    assert!(!prompt.system_prompt.contains("# Agent identity"));

    h.shutdown().expect("shutdown");
}

/// The built-in delegate-role prompt fragment follows the prompt-owned
/// `agent_start` capability and its late priority, so every role that can
/// delegate sees the available role catalog after harness guidance while roles
/// that cannot delegate keep their previous prompt.
#[test]
fn available_delegate_roles_prompt_follows_agent_start_capability() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.install_internal_tool_handlers(vec![std::sync::Arc::new(TestAgentStartBuiltin)]);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .as_deref()
        .map(crate::parse_agent_id)
        .expect("durable agent id");
    let role = h.config.selected_role.clone();
    let model = crate::model::model_for_role(
        &h.provider_runtime.model_info,
        &h.config.available_roles,
        &role,
    );

    let with_agent_start = h.gather_effective_tool_specs_for_role_model(&role, model.as_ref());
    let rendered = h
        .try_build_system_prompt_for_role_and_agent(
            &role,
            Some(&agent_id),
            Some(&agent_id),
            &with_agent_start,
            model.as_ref(),
            false,
        )
        .expect("render prompt with agent_start");
    assert!(rendered.contains("## Available agent roles for `agent_start`"));
    let catalog_offset = rendered
        .find("## Available agent roles for `agent_start`")
        .expect("delegate role heading");
    assert!(
        rendered.find("# Tau harness").expect("harness heading") < catalog_offset,
        "the role catalog must follow the harness guidance"
    );
    assert!(!rendered.contains("# Agent identity"));
    let catalog_rows = rendered
        .split_once("## Available agent roles for `agent_start`")
        .expect("delegate role heading")
        .1
        .lines()
        .skip_while(|line| line.is_empty())
        .take_while(|line| line.starts_with("* `"))
        .collect::<Vec<_>>();
    assert_eq!(
        catalog_rows,
        vec![
            "* `engineer` - \"Capable individual contributor. Good default for most tasks.\"",
            "* `engineer-junior` - \"Lower-reasoning but fast individual contributor. Best for straightforward coding tasks.\"",
            "* `engineer-senior` - \"Slow and expensive individual contributor using higher reasoning. Use only for the hardest tasks without prior planning, or when specifically requested.\"",
        ]
    );

    h.config
        .available_roles
        .get_mut(&role)
        .expect("selected role")
        .disable_tools
        .push(ToolName::new("agent_start"));
    let without_agent_start = h.gather_effective_tool_specs_for_role_model(&role, model.as_ref());
    let rendered = h
        .try_build_system_prompt_for_role_and_agent(
            &role,
            Some(&agent_id),
            Some(&agent_id),
            &without_agent_start,
            model.as_ref(),
            false,
        )
        .expect("render prompt without agent_start");
    assert!(!rendered.contains("## Available agent roles for `agent_start`"));
    assert!(!rendered.contains("* `engineer` - \"Capable individual contributor."));

    h.shutdown().expect("shutdown");
}

/// Effective `agent_start` snapshots expose only sorted visible roles. Hidden
/// roles remain callable, while model-unavailable roles stay excluded from
/// provider definitions and custom system-prompt templates.
#[test]
fn agent_start_definition_lists_visible_available_roles_without_prompt_catalog() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.install_internal_tool_handlers(vec![std::sync::Arc::new(TestAgentStartBuiltin)]);
    let role = h.config.selected_role.clone();
    h.config.available_roles.insert(
        "alpha".to_owned(),
        path_tau_config_settings::AgentRole::default(),
    );
    h.config.available_roles.insert(
        "unavailable".to_owned(),
        tau_config::settings::AgentRole {
            model: Some("missing/model".into()),
            ..Default::default()
        },
    );
    h.config
        .available_roles
        .get_mut("engineer-senior")
        .expect("built-in senior role")
        .visible = Some(false);
    h.prompt_coordination
        .context_discovery
        .system_prompt_templates
        .insert("no-fragments".to_owned(), "CUSTOM TEMPLATE".to_owned());
    h.config
        .available_roles
        .get_mut(&role)
        .expect("selected role")
        .prompt_override = Some("no-fragments".to_owned());

    let model = crate::model::model_for_role(
        &h.provider_runtime.model_info,
        &h.config.available_roles,
        &role,
    );
    let specs = h.gather_effective_tool_specs_for_role_model(&role, model.as_ref());
    let description = specs
        .iter()
        .find(|spec| spec.name.as_str() == "agent_start")
        .and_then(|spec| spec.description.as_deref());
    assert_eq!(
        description,
        Some("test agent_start. Roles: alpha, engineer, engineer-junior")
    );
    assert!(
        !description
            .expect("agent_start description")
            .contains("unavailable")
    );
    assert!(
        !description
            .expect("agent_start description")
            .contains("engineer-senior")
    );
    h.config.available_roles.insert(
        "beta".to_owned(),
        path_tau_config_settings::AgentRole::default(),
    );
    let specs = h.gather_effective_tool_specs_for_role_model(&role, model.as_ref());
    let description = specs
        .iter()
        .find(|spec| spec.name.as_str() == "agent_start")
        .and_then(|spec| spec.description.as_deref());
    assert_eq!(
        description,
        Some("test agent_start. Roles: alpha, beta, engineer, engineer-junior")
    );

    let agent_id = crate::parse_agent_id(
        h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role)
            .as_str(),
    );
    let system_prompt = h
        .try_build_system_prompt_for_role_and_agent(
            &role,
            Some(&agent_id),
            Some(&agent_id),
            &specs,
            model.as_ref(),
            false,
        )
        .expect("render custom template");
    assert_eq!(system_prompt, "CUSTOM TEMPLATE");

    h.submit_user_prompt(test_session_id("s1"), "hello".to_owned())
        .expect("submit prompt");
    let provider_prompt = read_nth_prompt_created(&h, 0);
    let provider_description = provider_prompt
        .tools
        .iter()
        .find(|tool| tool.name.as_str() == "agent_start")
        .and_then(|tool| tool.description.as_deref());
    assert_eq!(provider_description, description);
    let preview_description = h
        .gather_tool_definitions_for_role(&role)
        .into_iter()
        .find(|tool| tool.name.as_str() == "agent_start")
        .and_then(|tool| tool.description);
    assert_eq!(preview_description.as_deref(), description);

    h.config
        .available_roles
        .get_mut(&role)
        .expect("selected role")
        .disable_tools
        .push(ToolName::new("agent_start"));
    assert!(
        !h.gather_effective_tool_specs_for_role_model(&role, model.as_ref())
            .iter()
            .any(|spec| spec.name.as_str() == "agent_start")
    );

    for role in h.config.available_roles.values_mut() {
        role.visible = Some(false);
    }
    h.config
        .available_roles
        .get_mut(&role)
        .expect("selected role")
        .disable_tools
        .retain(|tool| tool.as_str() != "agent_start");
    let zero_visible_description = h
        .gather_effective_tool_specs_for_role_model(&role, model.as_ref())
        .into_iter()
        .find(|spec| spec.name.as_str() == "agent_start")
        .and_then(|spec| spec.description);
    assert_eq!(
        zero_visible_description.as_deref(),
        Some("test agent_start")
    );

    h.shutdown().expect("shutdown");
}

/// A first user prompt mints its durable agent identity lazily, so the delegate
/// role context must be published before that prompt is rendered.
#[test]
fn first_effective_prompt_with_agent_start_lists_delegate_roles() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.install_internal_tool_handlers(vec![std::sync::Arc::new(TestAgentStartBuiltin)]);

    h.submit_user_prompt(test_session_id("s1"), "hello".to_owned())
        .expect("submit first user prompt");

    let prompt = read_nth_prompt_created(&h, 0);
    assert!(
        prompt
            .system_prompt
            .contains("## Available agent roles for `agent_start`")
    );
    assert!(prompt.system_prompt.contains(
        "* `engineer` - \"Capable individual contributor. Good default for most tasks.\""
    ));
    let catalog_offset = prompt
        .system_prompt
        .find("## Available agent roles for `agent_start`")
        .expect("delegate role heading");
    assert!(
        prompt
            .system_prompt
            .find("# Tau harness")
            .expect("harness heading")
            < catalog_offset,
        "provider prompt must retain late role-catalog placement"
    );

    h.shutdown().expect("shutdown");
}

/// A malformed template terminalizes a real create request's initial activation
/// without resurrecting its correlation after repair; a later prompt still
/// runs.
#[test]
fn malformed_prompt_template_blocks_then_retries_after_repair() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    h.extensions.enabled_names.insert("optional-ext".to_owned());
    let aliased = ToolSpec {
        name: ToolName::new("internal_alias"),
        model_visible_name: Some(ToolName::new("visible_alias")),
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    };
    let mut unsupported = aliased.clone();
    unsupported.name = ToolName::new("hidden_custom");
    unsupported.model_visible_name = None;
    unsupported.tool_type = tau_proto::ToolType::Custom;
    h.tool_routing
        .registry
        .register(&crate::test_connection_id("capability-test"), aliased);
    h.tool_routing
        .registry
        .register(&crate::test_connection_id("capability-test"), unsupported);
    let selected_role = h.config.selected_role.clone();
    h.prompt_coordination
        .context_discovery
        .system_prompt_templates
        .insert(
            "conditional-template".to_owned(),
            "{{tool_available capabilities.tools}}".to_owned(),
        );
    h.config
        .available_roles
        .entry(selected_role.clone())
        .or_default()
        .prompt_override = Some("conditional-template".to_owned());

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("malformed-template-ui"),
        tau_proto::UiCreateAgent {
            request_id: "create-malformed-template".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: selected_role.clone(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some("initial prompt".to_owned()),
            literal: false,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("prompt-malformed-template".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("create agent");
    let agent_id = h
        .agent_runtime
        .agent_registry
        .session_loaded
        .iter()
        .next()
        .cloned()
        .expect("created agent");
    let cid = h
        .agent_runtime
        .agent_registry
        .agents
        .iter()
        .find_map(|(cid, agent)| {
            (agent.identity.agent_id.as_deref() == Some(agent_id.as_str())).then(|| cid.clone())
        })
        .expect("created runtime agent");
    let prompt_count = |h: &Harness| {
        let mut cursor = path_crate_event_log::EventLogSeq::new(0);
        let mut count = 0;
        while let Some(record) = h.runtime_io.event_log.get_next_from(cursor) {
            cursor = record.seq.next();
            count += usize::from(matches!(record.event, Event::AgentPromptCreated(_)));
        }
        count
    };
    assert_eq!(prompt_count(&h), 0);
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptFailed(failed)
            if failed.request_id == "create-malformed-template"
                && failed.agent_id == agent_id
                && failed.ctx_id == "prompt-malformed-template"
                && failed.stage == tau_proto::AgentPromptFailureStage::Submission
    )));
    assert!(
        h.runtime_io
            .replayable_harness_notices
            .iter()
            .any(|notice| {
                notice.purpose == tau_proto::NoticePurpose::Alert
                    && notice.message.contains("until its template is repaired")
            })
    );
    assert_eq!(
        h.runtime_io
            .replayable_harness_notices
            .iter()
            .filter(|notice| notice.message.contains("until its template is repaired"))
            .count(),
        1
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_ctx_id
            .is_none()
    );

    h.prompt_coordination
        .context_discovery
        .system_prompt_templates
        .insert(
            "conditional-template".to_owned(),
            concat!(
                "READY alias={{tool_available capabilities.tools \"visible_alias\"}} ",
                "internal={{tool_available capabilities.tools \"internal_alias\"}} ",
                "unsupported={{tool_available capabilities.tools \"hidden_custom\"}} ",
                "enabled={{extension_enabled capabilities.extensions \"optional-ext\"}} ",
                "active={{extension_active capabilities.extensions \"optional-ext\"}}"
            )
            .to_owned(),
        );
    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::user("retry repaired template".to_owned())
            .with_ctx_id(Some("prompt-after-template-repair".to_owned())),
    )
    .expect("retry repaired template");
    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(
        prompt.system_prompt,
        "READY alias=true internal=false unsupported=false enabled=true active=false"
    );
    assert_eq!(
        prompt.ctx_id.as_deref(),
        Some("prompt-after-template-repair")
    );
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.ctx_id.as_deref() == Some("prompt-malformed-template")
    )));
    assert_eq!(prompt_count(&h), 1);
    h.shutdown().expect("shutdown");
}

/// An eager initial prompt and multiple durable message wakes must wait for
/// per-agent context, then coalesce into one dispatch with each message
/// materialized exactly once.
#[test]
fn eager_initial_prompt_waits_for_agent_context_before_strict_render() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    h.extensions.resolving_initial_collisions = true;

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("context-race-ui"),
        tau_proto::UiCreateAgent {
            request_id: "create-context-race".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: h.config.selected_role.clone(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some("render after workdir context".to_owned()),
            literal: false,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("prompt-context-race".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("create agent with queued initial prompt");
    let agent_id = h
        .agent_runtime
        .agent_registry
        .session_loaded
        .iter()
        .next()
        .cloned()
        .expect("created agent");
    let cid = h
        .runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
        .expect("created runtime agent");
    set_test_agent_context_wait(
        &mut h,
        agent_id.clone(),
        path_std_collections::HashSet::from([crate::test_connection_id("late-workdir-context")]),
    );
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("context-race-message")
                .expect("message id"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: agent_id.clone(),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "later durable message wake".to_owned(),
        }),
    );
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("context-race-message-two")
                .expect("message id"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: agent_id.clone(),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "second durable message wake".to_owned(),
        }),
    );

    h.extensions.resolving_initial_collisions = false;
    h.try_advance_queue();
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .any(|prompt| prompt.initial_prompt_correlation.is_some())
    );
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.ctx_id.as_deref() == Some("prompt-context-race")
    )));
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptFailed(failed) if failed.request_id == "create-context-race"
    )));
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentPromptCreated(_)))
    );

    publish_shell_workdir_context(
        &mut h,
        &agent_id,
        "workdir",
        "core-shell",
        "default",
        "/srv/context-ready",
        "available",
    );
    finish_test_agent_context_wait(&mut h, &agent_id);
    h.try_advance_queue();

    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(prompt.ctx_id.as_deref(), Some("prompt-context-race"));
    assert!(prompt.system_prompt.contains("/srv/context-ready"));
    let context = serde_json::to_string(&prompt.context).expect("serialize provider context");
    for message in ["later durable message wake", "second durable message wake"] {
        assert_eq!(context.match_indices(message).count(), 1);
    }
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        1
    );
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptFailed(failed) if failed.request_id == "create-context-race"
    )));
    h.shutdown().expect("shutdown");
}

/// A failed initial prompt is rejected before durable activation after context
/// readiness while an independently correlated ordinary B remains retryable
/// after surface repair.
#[test]
fn failed_create_prompt_preflight_preserves_later_prompt() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    h.extensions.resolving_initial_collisions = true;
    let tool_provider = crate::test_connection_id("watermark-race-tools");
    for internal_name in ["first_internal", "second_internal"] {
        h.tool_routing.registry.register(
            &tool_provider,
            ToolSpec {
                name: ToolName::new(internal_name),
                model_visible_name: Some(ToolName::new("duplicate_visible")),
                description: None,
                tool_type: tau_proto::ToolType::Function,
                parameters: None,
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            },
        );
    }

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("watermark-race-ui"),
        tau_proto::UiCreateAgent {
            request_id: "create-watermark-a".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: h.config.selected_role.clone(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some("initial A".to_owned()),
            literal: false,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("prompt-watermark-a".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("create agent with queued A");
    let agent_id = h
        .agent_runtime
        .agent_registry
        .session_loaded
        .iter()
        .next()
        .cloned()
        .expect("created agent");
    let cid = h
        .agent_runtime
        .agent_registry
        .agents
        .iter()
        .find_map(|(cid, agent)| {
            (agent.identity.agent_id.as_deref() == Some(agent_id.as_str())).then(|| cid.clone())
        })
        .expect("created runtime agent");
    let context_provider = crate::test_connection_id("watermark-race-context");
    set_test_agent_context_wait(
        &mut h,
        agent_id.clone(),
        path_std_collections::HashSet::from([context_provider]),
    );

    h.extensions.resolving_initial_collisions = false;
    h.try_advance_queue();
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .any(|prompt| prompt.initial_prompt_correlation.is_some())
    );
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptFailed(failed) if failed.request_id == "create-watermark-a"
    )));
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.ctx_id.as_deref() == Some("prompt-watermark-a")
    )));
    finish_test_agent_context_wait(&mut h, &agent_id);
    h.try_advance_queue();
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .pending_initial_correlations
            .contains_key(&cid)
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptFailed(failed)
            if failed.request_id == "create-watermark-a"
                && failed.ctx_id == "prompt-watermark-a"
    )));
    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::user("independent B".to_owned())
            .with_ctx_id(Some("prompt-watermark-b".to_owned())),
    )
    .expect("commit B after A terminal");
    let activation_b = h
        .runtime_io
        .publication
        .idle_dispatches
        .back()
        .and_then(|dispatch| dispatch.activation_through)
        .expect("B committed watermark");
    assert!(
        h.runtime_io
            .publication
            .idle_dispatches
            .iter()
            .any(|dispatch| { dispatch.activation_through == Some(activation_b) })
    );

    h.tool_routing
        .registry
        .unregister_connection(&tool_provider);
    h.drain_publish_idle_dispatches();
    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(prompt.ctx_id.as_deref(), Some("prompt-watermark-b"));
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.ctx_id.as_deref() == Some("prompt-watermark-a")
    )));
    h.shutdown().expect("shutdown");
}

/// A render-preflight failure leaves no durable activating submission, so cold
/// reload cannot reconstruct and execute the terminalized initial prompt.
#[test]
fn failed_create_prompt_does_not_resurrect_after_cold_reload() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let agent_id;
    {
        let mut h = echo_harness(&state).expect("start");
        h.config.selected_model = Some("test/model".into());
        let provider = crate::test_connection_id("cold-preflight-tools");
        for internal_name in ["first_internal", "second_internal"] {
            h.tool_routing.registry.register(
                &provider,
                ToolSpec {
                    name: ToolName::new(internal_name),
                    model_visible_name: Some(ToolName::new("duplicate_visible")),
                    description: None,
                    tool_type: tau_proto::ToolType::Function,
                    parameters: None,
                    format: None,
                    tags: Vec::new(),
                    enabled_by_default: true,
                    background_support: None,
                    examples: Vec::new(),
                },
            );
        }
        h.handle_ui_create_agent_from(
            &crate::test_connection_id("cold-preflight-ui"),
            tau_proto::UiCreateAgent {
                request_id: "create-cold-preflight".to_owned(),
                session_id: h.session_runtime.current_session_id.clone(),
                role: h.config.selected_role.clone(),
                model_override: None,
                metadata: Vec::new(),
                initial_prompt: Some("must not survive restart".to_owned()),
                literal: false,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: Some("prompt-cold-preflight".to_owned()),
                parent_agent: None,
                ephemeral: false,
            },
        )
        .expect("create agent");
        agent_id = h
            .agent_runtime
            .agent_registry
            .session_loaded
            .iter()
            .next()
            .cloned()
            .expect("created agent");
        assert!(event_log_events(&h).iter().all(|event| !matches!(
            event,
            Event::AgentPromptSubmitted(submitted)
                if submitted.agent_id == agent_id
                    && submitted.ctx_id.as_deref() == Some("prompt-cold-preflight")
        )));
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("cold resume");
    resumed.try_advance_queue();
    assert!(event_log_events(&resumed).iter().all(|event| !matches!(
        event,
        Event::AgentPromptCreated(prompt) if prompt.agent_id == agent_id
    )));
    resumed.shutdown().expect("resumed shutdown");
}

#[test]
fn queued_first_user_prompt_publishes_replayable_agent_target() {
    // Regression: if the first prompt queues before the provider/model is ready,
    // the agent id must already exist and be carried on the transient queued
    // event and in-memory replay so a live or late UI can select the same
    // conversation before dispatch. Queue lifecycle events are intentionally not
    // durable session-store facts.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.provider_runtime.model_routes.clear();
    h.provider_runtime.model_info.clear();
    h.provider_runtime.available_models.clear();
    h.config.selected_model = None;
    // Model-less queueing is transient only while startup still has an
    // unapplied extension connection.
    h.extensions.pending_connects = 1;

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "test-create-request".to_owned(),
            literal: false,
            parent_agent: None,
            session_id: test_session_id("s1"),
            role: h.config.selected_role.clone(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some("hello while cold".to_owned()),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("create-cold-queue-prompt".to_owned()),
            ephemeral: false,
        },
    )
    .expect("create agent with queued first prompt");

    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&test_user_agent(&h))
        .and_then(|conversation| conversation.identity.agent_id.as_deref())
        .expect("queued first prompt minted agent id")
        .to_owned();
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptQueued(queued)
            if queued.agent_id.as_str() == agent_id.as_str()
    )));
    let default_conversation = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&test_user_agent(&h))
        .expect("default conversation");
    assert_eq!(default_conversation.dispatch.pending_prompts.len(), 1);
    assert_eq!(
        default_conversation.dispatch.pending_prompts[0].text,
        "hello while cold"
    );
    assert!(
        loaded_agent_events(&h, "s1")
            .iter()
            .all(|event| !matches!(event, Event::AgentPromptQueued(_))),
        "queued prompts are transient and must not be persisted"
    );
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent_events(&agent_id)
            .expect("queued agent journal")
            .iter()
            .filter(|record| matches!(record.event, Event::AgentUserInteractionRecorded(_)))
            .count(),
        1,
        "accepted queued initial prompt records exactly one interaction fact"
    );

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");
    let ui_conn = h
        .runtime_io
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();
    h.handle_client_event(
        &ui_conn,
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Prefix("agent.".to_owned())],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut queued = Vec::new();
    while let Ok(Some(frame)) = reader.read_frame() {
        let inner = frame.into_event_frame();
        if let TestProtocolItem::Event(Event::AgentPromptQueued(event)) = inner {
            queued.push(event);
        }
    }
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].text, "hello while cold");
    assert_eq!(queued[0].agent_id.as_str(), agent_id.as_str());

    h.shutdown().expect("shutdown");
}

#[test]
fn resume_ignores_later_side_queued_or_steered_default_agent_candidates() {
    // Regression: queued/steered events do not carry an originator, so a later
    // side-conversation durable event must not steal the default conversation's
    // agent binding during cold resume.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    {
        let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
        let mut sessions = tau_core::SessionStore::open(&sessions_dir).expect("session store");
        sessions
            .record_session_meta("s1")
            .expect("seed canonical session manifest");
        for agent_id in ["engineer_default", "worker_steered"] {
            sessions
                .append_session_event(
                    "s1",
                    None,
                    Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                        agent_initialization_id: tau_proto::AgentInitializationId::parse(
                            "test-init",
                        )
                        .expect("test identifier must be valid"),

                        session_id: test_session_id("s1"),
                        agent_id: crate::parse_agent_id(agent_id),
                        ephemeral: false,
                    }),
                )
                .expect("seed session membership");
        }
        drop(sessions);

        let mut agents = tau_core::AgentStore::open(sp.join("agents")).expect("agent store");
        for agent_id in ["engineer_default", "worker_steered"] {
            agents
                .append_agent_event(
                    agent_id,
                    None,
                    Event::AgentStarted(tau_proto::AgentStarted {
                        creator: Some(tau_proto::AgentCreator::default()),

                        parent_agent: None,
                        agent_id: crate::parse_agent_id(agent_id),
                        role: "engineer".to_owned(),
                        display_name: None,
                        metadata: Vec::new(),
                        ephemeral: false,
                    }),
                )
                .expect("seed creation");
        }
        agents
            .append_agent_event(
                "engineer_default",
                None,
                Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                    inference_activation: false,
                    agent_id: tau_proto::AgentId::parse("engineer_default").expect("agent id"),
                    text: "default prompt".to_owned(),
                    trusted_internal_spans: Vec::new(),
                    message_class: tau_proto::PromptMessageClass::User,
                    internal_kind: None,
                    originator: tau_proto::PromptOriginator::User,
                    submission_source: Default::default(),
                    display_name: None,
                    ctx_id: None,
                }),
            )
            .expect("seed default prompt");
        agents
            .append_agent_event(
                "worker_steered",
                None,
                Event::AgentPromptSteered(AgentPromptSteered {
                    self_compaction_terminal: None,
                    inference_activation: false,
                    submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
                    agent_id: tau_proto::AgentId::parse("worker_steered").expect("agent id"),
                    text: "side steered".to_owned(),
                    trusted_internal_spans: Vec::new(),
                    message_class: tau_proto::PromptMessageClass::User,
                    internal_kind: None,
                    ctx_id: None,
                }),
            )
            .expect("seed side steered prompt");
    }

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&test_user_agent(&h))
            .and_then(|conversation| conversation.identity.agent_id.as_deref()),
        Some("engineer_default")
    );
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .agent_routes
            .get("engineer_default"),
        Some(&test_user_agent(&h))
    );
    assert_ne!(
        h.agent_runtime
            .agent_registry
            .agent_routes
            .get("worker_steered"),
        Some(&test_user_agent(&h))
    );
    h.shutdown().expect("shutdown");
}

/// If an old prompt calls a tool that was advertised with a strict schema but
/// whose provider has since disappeared, provider availability must be checked
/// before schema validation. The model should receive the accurate NoProvider
/// error instead of a misleading invalid-arguments error for a tool that cannot
/// run anymore.
#[test]
fn old_prompt_missing_provider_wins_over_strict_schema_validation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-strict-old-tool");
    let mut spec = shared_test_tool_spec("strict_old_tool");
    spec.parameters = Some(serde_json::json!({
        "type": "object",
        "properties": {
            "allowed": { "type": "string" }
        },
        "required": ["allowed"],
        "additionalProperties": false
    }));
    h.tool_routing
        .registry
        .register(&crate::test_connection_id("conn-strict-old-tool"), spec);

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-strict-old-tool");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.tool_routing
        .registry
        .unregister_connection(&crate::test_connection_id("conn-strict-old-tool"));

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "missing-strict".into(),
            name: ToolName::new("strict_old_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("extra".to_owned()),
                CborValue::Text("nope".to_owned()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("old prompt missing provider handled");

    let expected = unavailable_tool_error_message(&ToolName::new("strict_old_tool"));
    let mut provider_error = None;
    let mut logical_events = Vec::new();
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        match &entry.event {
            Event::ProviderToolError(error) if error.call_id.as_str() == "missing-strict" => {
                provider_error = Some(error.message.clone());
            }
            Event::ToolRequest(request) if request.call_id.as_str() == "missing-strict" => {
                logical_events.push("tool.request");
            }
            Event::ToolStarted(invoke) if invoke.call_id.as_str() == "missing-strict" => {
                logical_events.push("tool.started");
            }
            Event::ToolError(error) if error.call_id.as_str() == "missing-strict" => {
                logical_events.push("tool.error");
            }
            _ => {}
        }
    }

    assert_eq!(provider_error.as_deref(), Some(expected.as_str()));
    assert_eq!(logical_events, vec!["tool.request", "tool.error"]);
    assert!(tool_invoke_call_ids(&tool_events).is_empty());

    h.shutdown().expect("shutdown");
}

/// A synchronous terminal append fault during disconnect retains the batch for
/// retry without dispatching queued work to another live provider.
#[test]
fn disconnect_append_fault_retains_batch_without_draining_queued_work() {
    let (_td, mut h) = setup_routed_test_tool_call("disconnect-fault", "owned_tool");
    let cid = h.tool_routing.tool_runtime.tool_agents["disconnect-fault"].clone();
    let live_events = connect_test_tool(&mut h, "conn-live-after-fault");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-live-after-fault"),
        shared_test_tool_spec("live_after_fault"),
    );
    h.tool_routing.tool_runtime.tool_turn.push(
        cid,
        AgentToolCall {
            call_ref: None,
            id: "queued-after-fault".into(),
            name: ToolName::new("live_after_fault"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        },
        tau_proto::BackgroundSupport::Never,
    );

    reject_semantic_admissions(&h, 2);
    h.handle_disconnect(&crate::test_connection_id("conn-owner"));

    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 1);
    assert!(tool_invoke_call_ids(&live_events).is_empty());
    assert!(
        !h.runtime_io
            .publication
            .disconnect_terminal_batch_pending
            .is_empty()
    );
    assert!(
        h.runtime_io
            .publication
            .disconnect_terminal_batch_completed
            .is_empty()
    );
}
