//! Tests for cancellation and background behavior.

use super::super::super::CancelTarget;
use super::*;

/// Background terminal events are harness-derived records. Extensions must not
/// be able to inject them directly into an agent log.
#[test]
fn provider_owner_validation_rejects_external_background_result() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-background-call", "owned_tool");

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-wrong"),
        Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
            call_id: "owner-background-call".into(),
            tool_name: ToolName::new("owned_tool"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("spoofed background".to_owned()),
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
    )
    .expect("wrong background result ignored");

    assert!(
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("owner-background-call")
    );
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolBackgroundResult(result) if result.call_id.as_str() == "owner-background-call"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_external_background_error() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-background-error-call", "owned_tool");

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-wrong"),
        Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
            call_id: "owner-background-error-call".into(),
            tool_name: ToolName::new("owned_tool"),
            tool_type: tau_proto::ToolType::Function,
            message: "spoofed background error".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
    )
    .expect("wrong background error ignored");

    assert!(
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("owner-background-error-call")
    );
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "owner-background-error-call"
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: a backgrounded call remains tracked after its synthetic
/// placeholder closes the foreground, while later tool calls dispatch normally.
/// The real background result must clear only the actual-running state.
#[test]
fn background_result_clears_actual_running_call_without_blocking_later_tool() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let tool_events = connect_ready_configured_extension(
        &mut h,
        "conn-bg-result-drain",
        "configured-conn-bg-result-drain",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-result-drain"),
        scheduled_test_tool_spec("bg_update", tau_proto::BackgroundSupport::Instant),
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-result-drain"),
        scheduled_test_tool_spec("queued_update", tau_proto::BackgroundSupport::Never),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-bg-result-drain");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "bg-update-running".into(),
                name: ToolName::new("bg_update"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-update".into(),
                name: ToolName::new("queued_update"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
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
    .expect("tool response");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["bg-update-running".to_owned(), "queued-update".to_owned()]
    );
    assert_eq!(background_placeholder_count(&h, "bg-update-running"), 1);
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&"bg-update-running".into())
    );
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-bg-result-drain"),
        Event::ToolResultReported(final_tool_result(
            "bg-update-running",
            "bg_update",
            "background output",
        )),
    )
    .expect("background result accepted");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["bg-update-running".to_owned(), "queued-update".to_owned()]
    );
    assert_eq!(background_result_count(&h, "bg-update-running"), 1);
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&"bg-update-running".into())
    );
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("bg-update-running")
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("queued-update")
    );

    h.shutdown().expect("shutdown");
}

/// Regression: disconnect cleanup can synthesize errors for more than one
/// backgrounded call from the same dead provider without touching unrelated
/// calls that have already dispatched to another provider.
#[test]
fn disconnect_background_errors_do_not_affect_other_inflight_tools() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _dead_events = connect_test_tool(&mut h, "conn-bg-disconnect-batch");
    let live_events = connect_test_tool(&mut h, "conn-bg-disconnect-live");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-disconnect-batch"),
        scheduled_test_tool_spec("dead_bg_shared", tau_proto::BackgroundSupport::Instant),
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-disconnect-batch"),
        scheduled_test_tool_spec("dead_bg_update", tau_proto::BackgroundSupport::Instant),
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-disconnect-live"),
        scheduled_test_tool_spec("live_queued_update", tau_proto::BackgroundSupport::Never),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-bg-disconnect-batch");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "b-bg-shared".into(),
                name: ToolName::new("dead_bg_shared"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "a-bg-update".into(),
                name: ToolName::new("dead_bg_update"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "z-queued-update".into(),
                name: ToolName::new("live_queued_update"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
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
    .expect("tool response");

    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .get("b-bg-shared")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-bg-disconnect-batch")
    );
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .get("a-bg-update")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-bg-disconnect-batch")
    );
    assert_eq!(
        tool_invoke_call_ids(&live_events),
        vec!["z-queued-update".to_owned()]
    );
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&"a-bg-update".into())
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&"b-bg-shared".into())
    );

    h.handle_disconnect(&crate::test_connection_id("conn-bg-disconnect-batch"));

    assert_eq!(
        tool_invoke_call_ids(&live_events),
        vec!["z-queued-update".to_owned()]
    );
    assert_eq!(background_error_count(&h, "a-bg-update"), 1);
    assert_eq!(background_error_count(&h, "b-bg-shared"), 1);
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("a-bg-update")
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("b-bg-shared")
    );
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .get("z-queued-update")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-bg-disconnect-live")
    );

    h.shutdown().expect("shutdown");
}

/// A terminal cancellation from a non-owner must not poison the tool round
/// before the routed provider returns the real result.
#[test]
fn provider_owner_validation_rejects_wrong_tool_cancelled() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-cancelled-call", "owned_tool");

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-wrong"),
        Event::ToolCancelledReported(tau_proto::ToolCancelled {
            presentation: Default::default(),
            call_id: "owner-cancelled-call".into(),
            tool_name: ToolName::new("owned_tool"),
            tool_type: tau_proto::ToolType::Function,
            display: None,
        }),
    )
    .expect("wrong cancellation ignored");

    assert!(
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("owner-cancelled-call")
    );
    assert!(event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolCancelledReported(cancelled)
            if cancelled.call_id.as_str() == "owner-cancelled-call"
    )));

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-owner"),
        Event::ToolResultReported(final_tool_result(
            "owner-cancelled-call",
            "owned_tool",
            "real output",
        )),
    )
    .expect("owner result accepted");

    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("owner-cancelled-call")
    );
    assert!(event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ToolResult(result)
                if result.call_id.as_str() == "owner-cancelled-call"
                    && matches!(&result.result, CborValue::Text(text) if text == "real output")
        )
    ));

    h.shutdown().expect("shutdown");
}

/// Cancelling a routed tool publishes the durable broadcast cancellation
/// request and the local terminal `ToolCancelled` event. Extensions observe the
/// event log instead of receiving point-to-point cancellation frames.
#[test]
fn cancel_publishes_tool_cancel_request() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _owner_events = connect_test_tool(&mut h, "conn-cancel-owner");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-cancel-owner"),
        shared_test_tool_spec("cancel_tool"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-cancel-tool");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    let target_agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&target_agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "cancel-call".into(),
            name: ToolName::new("cancel_tool"),
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
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool call routed");

    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
            agent_prompt_id: None,
        },
    );

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelRequest(request) if request.target_call_id.as_str() == "cancel-call"
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "cancel-call"
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: live user cancellation of a turn with an already-backgrounded
/// call must keep a queued internal completion notice on the live branch. The
/// notice should not auto-advance immediately, otherwise canceling a turn could
/// cause the model to restart solely to observe harness-authored cancellation.
#[test]
fn live_cancel_backgrounded_tool_queues_completion_notice_without_advancing() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let cid = ensure_test_user_agent(&mut h);
    publish_test_tool_declaration(&mut h, &cid, "live-cancel-bg-call");
    let target_agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    let call_id: ToolCallId = "live-cancel-bg-call".into();
    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("live_cancel_bg_tool"),
            internal_name: ToolName::new("live_cancel_bg_tool"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), cid.clone());
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(cid.clone(), call_id.clone(), ToolTurnCategories::default());
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .begin_backgrounding(&call_id)
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .mark_backgrounded(&call_id)
    );
    h.publish_synthetic_background_result(&call_id);
    seed_tools_running(&mut h, &cid, vec![call_id.clone()]);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("conversation")
        .dispatch
        .pending_prompts
        .push_back(PendingPrompt::user(
            "queued user prompt to discard".to_owned(),
        ));

    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
            agent_prompt_id: None,
        },
    );

    let completion_prompt = background_completion_prompt(&call_id);
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::ToolBackgroundError(error) if error.call_id == call_id
        )),
        1,
        "events: {:?}",
        event_log_events(&h)
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::Idle
    ));
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_cancel
            .is_none()
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .any(|prompt| prompt.text == completion_prompt
                && prompt.is_passive_background_completion()),
        "live branch should retain the internal background completion notice"
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .all(PendingPrompt::is_passive_background_completion),
        "live cancel should still discard stale non-internal queued prompts"
    );
    h.try_advance_queue();
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == completion_prompt
    )));
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::Idle
    ));

    let submission = h
        .submit_user_prompt(test_session_id("s1"), "continue after cancel".to_owned())
        .expect("submit follow-up user prompt");
    assert_eq!(submission, PromptSubmission::Dispatched);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
            Event::AgentPromptSubmitted(submitted)
            if submitted.text == completion_prompt
                && submitted.message_class == tau_proto::PromptMessageClass::Internal
                && submitted.internal_kind
                    == Some(tau_proto::InternalPromptKind::BackgroundToolCompletion)
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text == "continue after cancel"
                && submitted.message_class == tau_proto::PromptMessageClass::User
    )));
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != completion_prompt),
        "follow-up user prompt should consume the passive background notice"
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tools
            .contains_key(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&call_id)
    );

    h.shutdown().expect("shutdown");
}

/// Regression: live cancellation must include backgrounded calls that have
/// already left `ToolsRunning.remaining_calls` while a sibling foreground tool
/// keeps the same turn active. Otherwise the background placeholder would stay
/// unresolved until the background task eventually reported back.
#[test]
fn live_cancel_tools_running_includes_already_backgrounded_siblings() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-live-mixed-cancel");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-live-mixed-cancel"),
        instant_background_test_tool_spec("live_bg_tool"),
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-live-mixed-cancel"),
        shared_test_tool_spec("live_foreground_tool"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-live-mixed-cancel");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    let target_agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    let bg_call_id: ToolCallId = "live-bg-sibling".into();
    let fg_call_id: ToolCallId = "live-fg-sibling".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&target_agent_id),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: bg_call_id.clone(),
                name: ToolName::new("live_bg_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: fg_call_id.clone(),
                name: ToolName::new("live_foreground_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
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
    .expect("mixed tool turn starts");

    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&bg_call_id)
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .is_empty()
    );

    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
            agent_prompt_id: None,
        },
    );

    assert_eq!(background_error_count(&h, bg_call_id.as_str()), 1);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id == fg_call_id
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id == bg_call_id
    )));
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .any(|prompt| {
                prompt.text == background_completion_prompt(&bg_call_id)
                    && prompt.is_passive_background_completion()
            }),
        "backgrounded sibling should receive a passive completion notice"
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&bg_call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tools
            .contains_key(&bg_call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tools
            .contains_key(&fg_call_id)
    );

    h.shutdown().expect("shutdown");
}

/// Late progress for a backgrounded tool must not be published. The foreground
/// tool result has already closed the visible tool block, so orphan progress
/// would render as confusing standalone text like `shell: running shell
/// command`.
#[test]
fn backgrounded_tool_progress_is_not_published() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _ = connect_ready_configured_extension(
        &mut h,
        "conn-slow",
        "configured-slow",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-slow"),
        ToolSpec {
            name: ToolName::new("slow"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
            examples: Vec::new(),
        },
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-bg-progress");
    seed_agent_thinking(&mut h, &cid, "sp-bg-progress");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "run slow".to_owned(),
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

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "slow-call".into(),
            name: ToolName::new("slow"),
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
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("background tool call");

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-slow"),
        Event::ToolProgressReported(tau_proto::ToolProgress {
            call_id: "slow-call".into(),
            tool_name: ToolName::new("slow"),
            message: Some("running shell command".to_owned()),
            progress: None,
            display: None,
        }),
    )
    .expect("late progress");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolProgress(progress) if progress.call_id.as_str() == "slow-call"
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: a background placeholder without a later background result/error
/// means the real tool was lost across cold restore. Resume must publish a
/// durable background error, fold an internal interruption note before the next
/// user prompt, and let `wait` consume the restored error instead of hanging.
#[test]
fn resumed_lost_background_tool_gets_error_and_wait_returns() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "lost-bg", "slow_bg");

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let notice = restored_background_notice("lost-bg");

    assert_eq!(background_error_count(&h, "lost-bg"), 1);
    assert!(event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ToolBackgroundError(error)
                if error.call_id.as_str() == "lost-bg" && error.message == notice
        )
    ));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(_)
    )));

    h.submit_user_prompt(test_session_id("s1"), "after restore".to_owned())
        .expect("submit first resumed prompt");
    let first_prompt = read_nth_prompt_created(&h, 0);
    let first_spid = first_prompt.agent_prompt_id.clone();
    let notice_pos = first_prompt
        .context
        .flatten()
        .iter()
        .position(|item| text_part(item) == Some(crate::internal_envelope::frame(&notice).as_str()))
        .expect("background interruption notice in first prompt");
    let user_pos = first_prompt
        .context
        .flatten()
        .iter()
        .position(|item| text_part(item) == Some("after restore"))
        .expect("user prompt in first prompt");
    assert!(notice_pos < user_pos);

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: first_spid,
        agent_id: first_prompt.agent_id.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "wait-lost-bg".into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text("lost-bg".to_owned()),
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
    .expect("wait for restored background call");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-lost-bg" && error.message == notice
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: when an idle conversation has more than one backgrounded call on
/// a disconnected provider, the harness must record every synthetic background
/// error before it dispatches the first internal completion prompt back to the
/// model. Dispatching after the first error would let the follow-up miss later
/// failures from the same disconnect batch.
#[test]
fn disconnect_idle_multi_background_errors_dispatch_prompt_after_batch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _dead_events = connect_test_tool(&mut h, "conn-bg-idle-disconnect");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-idle-disconnect"),
        scheduled_test_tool_spec("dead_bg_one", tau_proto::BackgroundSupport::Instant),
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-idle-disconnect"),
        scheduled_test_tool_spec("dead_bg_two", tau_proto::BackgroundSupport::Instant),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-bg-idle-disconnect");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "a-bg-idle".into(),
                name: ToolName::new("dead_bg_one"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "b-bg-idle".into(),
                name: ToolName::new("dead_bg_two"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
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
    .expect("background tool response");

    let followup_spid = match &h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("conversation remains live")
        .turn
        .turn_state
    {
        AgentTurnState::AgentThinking { agent_prompt_id } => agent_prompt_id.clone(),
        state => panic!("expected placeholder follow-up prompt, got {state:?}"),
    };
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_provider_response_finished(provider_text_response(
        &followup_spid,
        agent_id,
        "placeholders observed",
    ))
    .expect("finish placeholder follow-up");
    assert!(matches!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("conversation remains live")
            .turn
            .turn_state,
        AgentTurnState::Idle
    ));

    h.handle_disconnect(&crate::test_connection_id("conn-bg-idle-disconnect"));

    let first_error_seq = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolBackgroundError(error) if error.call_id.as_str() == "a-bg-idle"
        )
    })
    .expect("first background error");
    let second_error_seq = event_log_position_after(&h, first_error_seq, |event| {
        matches!(
            event,
            Event::ToolBackgroundError(error) if error.call_id.as_str() == "b-bg-idle"
        )
    })
    .expect("second background error");
    let prompt_after_first_error_seq = event_log_position_after(&h, first_error_seq, |event| {
        matches!(event, Event::AgentPromptCreated(_))
    })
    .expect("background completion follow-up prompt");
    assert!(second_error_seq < prompt_after_first_error_seq);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text == background_completion_prompt(&"a-bg-idle".into())
                && submitted.internal_kind
                    == Some(tau_proto::InternalPromptKind::BackgroundToolCompletion)
    )));

    h.shutdown().expect("shutdown");
}

/// A disconnect batch with foreground and background calls must finish in full
/// before repairing a stale live projection and committing one successor.
#[test]
fn disconnect_mixed_foreground_and_background_errors_dispatch_prompt_after_batch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let dead_events = connect_test_tool(&mut h, "conn-mixed-disconnect");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-mixed-disconnect"),
        scheduled_test_tool_spec("dead_foreground", tau_proto::BackgroundSupport::Never),
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-mixed-disconnect"),
        scheduled_test_tool_spec("dead_background", tau_proto::BackgroundSupport::Instant),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-mixed-disconnect");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "a-foreground-disconnect".into(),
                name: ToolName::new("dead_foreground"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "b-background-disconnect".into(),
                name: ToolName::new("dead_background"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
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
    .expect("mixed tool response");

    assert_eq!(
        tool_invoke_call_ids(&dead_events),
        vec![
            "a-foreground-disconnect".to_owned(),
            "b-background-disconnect".to_owned(),
        ]
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&"b-background-disconnect".into())
    );
    assert!(matches!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("conversation remains live")
            .turn
            .turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));
    let AgentTurnState::ToolsRunning { remaining_calls } = &mut h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("conversation")
        .turn
        .turn_state
    else {
        unreachable!("asserted tools-running state");
    };
    remaining_calls.push("stale-disconnect-projection".into());

    h.handle_disconnect(&crate::test_connection_id("conn-mixed-disconnect"));

    let foreground_error_seq = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolError(error) if error.call_id.as_str() == "a-foreground-disconnect"
        )
    })
    .expect("foreground synthetic error");
    let background_error_seq = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolBackgroundError(error)
                if error.call_id.as_str() == "b-background-disconnect"
        )
    })
    .expect("background synthetic error");
    let prompt_after_foreground_error_seq =
        event_log_position(&h, |event| matches!(event, Event::AgentPromptCreated(_)))
            .expect("post-disconnect follow-up prompt");
    assert!(foreground_error_seq < prompt_after_foreground_error_seq);
    assert!(background_error_seq < prompt_after_foreground_error_seq);
    let successor = read_nth_prompt_created(&h, 0);
    assert_eq!(
        agent_event_count(&h, |event| matches!(
            event,
            Event::ProviderToolError(error)
                if error.call_id.as_str() == "a-foreground-disconnect"
        )),
        1,
        "disconnect repair must not duplicate the canonical foreground terminal"
    );
    assert_eq!(
        agent_event_count(&h, |event| matches!(
            event,
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == successor.agent_prompt_id
        )),
        1,
        "disconnect repair must commit one durable successor"
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );

    h.shutdown().expect("shutdown");
}

/// Cancellation of exact ordinary inference releases its runtime checkpoint,
/// emits one transient terminal, and discards a later provider response.
#[test]
fn cancel_while_thinking_terminates_prompt_and_drops_late_response() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-cancel-thinking");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("conversation")
        .dispatch
        .in_flight_prompt = Some(spid.clone());
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("conversation")
        .dispatch
        .activation_dispatch = path_crate_agent::ActivationDispatchState::DispatchUncertain {
        owner: path_crate_agent::InferenceCheckpointOwner::Inference,
        agent_prompt_id: spid.clone(),
        through: tau_proto::AgentHead::Root,
        model: Some("test/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(tau_proto::AgentHead::Root),
    };
    let target_agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());

    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
            agent_prompt_id: None,
        },
    );

    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::Idle
    ));
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .in_flight_prompt
            .is_none()
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_cancel
            .is_none()
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert!(h.prompt_coordination.canceled_prompts.contains(&spid));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptTerminated(terminated)
            if terminated.agent_prompt_id == spid
                && terminated.reason == tau_proto::AgentPromptTerminationReason::Canceled
    )));
    let response_count_before = event_log_events(&h)
        .into_iter()
        .filter(|event| matches!(event, Event::ProviderResponseFinished(_)))
        .count();

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        agent_prompt_id: spid.clone(),
        agent_id: crate::parse_agent_id(&target_agent_id),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "(cancelled by harness)".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        provider_attempt: Default::default(),
        usage: Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens: 1_000_000,
            ..tau_proto::ProviderTokenUsage::default()
        }),
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("late canceled response should be ignored");

    let response_count_after = event_log_events(&h)
        .into_iter()
        .filter(|event| matches!(event, Event::ProviderResponseFinished(_)))
        .count();
    assert_eq!(response_count_after, response_count_before);
    assert!(!h.prompt_coordination.canceled_prompts.contains(&spid));
    assert_eq!(
        h.agent_stats_snapshot(&cid)
            .expect("agent stats")
            .estimated_api_cost,
        tau_proto::EstimatedApiCost::default(),
        "a late canceled terminal must not charge the agent"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn background_notification_suppression_keeps_error_event_but_skips_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _ = connect_ready_configured_extension(
        &mut h,
        "conn-fail",
        "configured-conn-fail",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-fail"),
        ToolSpec {
            name: ToolName::new("fail"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
            examples: Vec::new(),
        },
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-bg-error");
    seed_agent_thinking(&mut h, &cid, "sp-bg-error");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "run fail".to_owned(),
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

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "fail-call".into(),
            name: ToolName::new("fail"),
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
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("background tool call");

    h.suppress_background_completion_prompt("fail-call".into());
    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-fail"),
        Event::ToolErrorReported(tau_proto::ToolError {
            presentation: Default::default(),
            call_id: "fail-call".into(),
            tool_name: ToolName::new("fail"),
            tool_type: tau_proto::ToolType::Function,
            message: "late failure".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
    )
    .expect("late tool error");

    assert!(event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ToolBackgroundError(error)
                if error.call_id.as_str() == "fail-call" && error.message == "late failure"
        )
    ));
    assert!(!event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ToolError(error) if error.call_id.as_str() == "fail-call"
        )
    ));
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("conversation remains live");
    assert!(
        conv.dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&"fail-call".into()))
    );

    h.shutdown().expect("shutdown");
}

/// If a wait is interrupted before the background call finishes, unsuppressing
/// first should let the later completion queue the normal internal prompt.
#[test]
fn background_notification_unsuppress_before_completion_allows_later_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-unsuppress-before".into();

    h.suppress_background_completion_prompt(call_id.clone());
    h.unsuppress_background_completion_prompt(call_id.clone());

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("default conversation remains live")
        .turn
        .turn_state = AgentTurnState::ToolsRunning {
        remaining_calls: Vec::new(),
    };
    h.tool_routing
        .tool_runtime
        .background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.queue_background_completion_prompt(&cid, &call_id);

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("default conversation remains live");
    assert!(conv.dispatch.pending_prompts.iter().any(|prompt| {
        prompt.text == background_completion_prompt(&call_id) && prompt.is_internal()
    }));

    h.shutdown().expect("shutdown");
}

/// If the real background completion arrives while suppressed, unsuppressing
/// later should restore the completion prompt from the recorded target map.
#[test]
fn background_notification_unsuppress_after_suppressed_completion_queues_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-unsuppress-after".into();

    h.suppress_background_completion_prompt(call_id.clone());
    h.tool_routing
        .tool_runtime
        .background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.queue_background_completion_prompt(&cid, &call_id);
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("default conversation remains live")
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("default conversation remains live")
        .turn
        .turn_state = AgentTurnState::ToolsRunning {
        remaining_calls: Vec::new(),
    };
    h.unsuppress_background_completion_prompt(call_id.clone());

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("default conversation remains live");
    assert!(conv.dispatch.pending_prompts.iter().any(|prompt| {
        prompt.text == background_completion_prompt(&call_id) && prompt.is_internal()
    }));

    h.shutdown().expect("shutdown");
}

/// Completed background calls remain in the target map so repeated wait cycles
/// can remove and then re-add the queued internal completion prompt.
#[test]
fn background_notification_repeated_suppress_unsuppress_after_completion_requeues_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-repeat".into();

    h.tool_routing
        .tool_runtime
        .background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.queue_background_completion_prompt(&cid, &call_id);
    h.suppress_background_completion_prompt(call_id.clone());
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("default conversation remains live")
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.unsuppress_background_completion_prompt(call_id.clone());
    h.suppress_background_completion_prompt(call_id.clone());
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("default conversation remains live")
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.unsuppress_background_completion_prompt(call_id.clone());
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("default conversation remains live");
    let prompt_count = conv
        .dispatch
        .pending_prompts
        .iter()
        .filter(|prompt| prompt.text == background_completion_prompt(&call_id))
        .count();
    assert_eq!(prompt_count, 1);

    h.shutdown().expect("shutdown");
}

/// Suppression can arrive after a background completion prompt was queued but
/// before the agent saw it; in that case the queued internal prompt is removed.
#[test]
fn background_notification_suppression_removes_queued_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg".into();

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("default conversation exists")
        .dispatch
        .pending_prompts
        .push_back(PendingPrompt::internal(background_completion_prompt(
            &call_id,
        )));
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("default conversation exists")
            .dispatch
            .pending_prompts
            .iter()
            .any(|prompt| prompt.text == background_completion_prompt(&call_id))
    );

    h.suppress_background_completion_prompt(call_id.clone());
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("default conversation exists")
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.shutdown().expect("shutdown");
}

/// A no-arg wait that is already blocked when its background call completes
/// must consume the result and suppress the normal internal completion prompt.
#[test]
fn no_arg_wait_before_background_completion_suppresses_completion_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _tool_events = connect_ready_configured_extension(
        &mut h,
        "conn-bg-any-before",
        "configured-conn-bg-any-before",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-any-before"),
        instant_background_test_tool_spec("slow_any_before"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-any-before".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        call_id.as_str(),
        "slow_any_before",
    );

    let wait_call = wait_no_args_call("wait-any-before");
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("start no-arg wait");
    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-bg-any-before"),
        Event::ToolResultReported(final_tool_result(
            call_id.as_str(),
            "slow_any_before",
            "background done",
        )),
    )
    .expect("background result");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-any-before"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some(call_id.as_str())
                && cbor_map_text(&result.result, "output") == Some("background done")
    )));
    let completion_prompt = background_completion_prompt(&call_id);
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered) if steered.text == completion_prompt
    )));
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != completion_prompt)
    );

    h.shutdown().expect("shutdown");
}

/// A completion notice must state that its result remains queued, and
/// `wait({})` must return that retained result while removing the notice.
#[test]
fn no_arg_wait_after_background_completion_removes_queued_completion_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-any-after".into();

    h.tool_routing
        .tool_runtime
        .background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.record_wait_background_result(
        tau_proto::ToolBackgroundResult {
            call_id: call_id.clone(),
            tool_name: ToolName::new("slow_any_after"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("already done".to_owned()),
            originator: tau_proto::PromptOriginator::User,

            display: None,
        },
        Some(tau_proto::ObservationId::random()),
    );
    seed_tools_running(&mut h, &cid, Vec::new());
    h.queue_background_completion_prompt(&cid, &call_id);
    let completion_prompt = background_completion_prompt(&call_id);
    assert_eq!(
        completion_prompt,
        "Tool call `bg-any-after` completed. Its result is queued; use `wait` to consume it."
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .any(|prompt| prompt.text == completion_prompt && prompt.is_internal())
    );

    let wait_call = wait_no_args_call("wait-any-after");
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("consume queued completion");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-any-after"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some(call_id.as_str())
                && cbor_map_text(&result.result, "output") == Some("already done")
    )));
    assert!(
        h.tool_routing
            .tool_runtime
            .suppressed_background_completion_prompts
            .contains(&call_id)
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != completion_prompt)
    );

    h.shutdown().expect("shutdown");
}

/// Background tool completion stays with a preserved tool-backed delegate.
///
/// A sub-agent can finish after its foreground receives the synthetic
/// background placeholder while the real tool is still running. Tool-backed
/// delegate agents are now detached instead of removed at completion, so
/// the late completion prompt must remain owned by the delegate conversation;
/// otherwise a resumed delegate could not receive results from tools it
/// started.
#[test]
fn background_completion_from_preserved_delegate_queues_on_delegate() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: ToolName::new("agent_start"),
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
    let _ = connect_ready_configured_extension(
        &mut h,
        "conn-slow",
        "configured-conn-slow",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-slow"),
        ToolSpec {
            name: ToolName::new("slow"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
            examples: Vec::new(),
        },
    );
    let parent_cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = test_agent_prompt_id("sp-main");
    seed_agent_thinking(&mut h, &parent_cid, "sp-main");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(main_spid.clone(), parent_cid.clone());
    h.publish_for_agent(
        &parent_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "delegate slow work".to_owned(),
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
            call_id: "delegate-call".into(),
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
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main delegate call");

    let mut query = ext_query("q-bg");
    query.tool_call_id = Some("delegate-call".into());
    h.handle_start_agent_request(&crate::test_connection_id("conn-delegate"), query)
        .expect("side query");
    let side_cid = ext_query_cid(&h, "q-bg").expect("side conversation");
    let side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "slow-call".into(),
            name: ToolName::new("slow"),
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
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("core-subagents"),
            query_id: "q-bg".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side tool call");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolResult(result)
            if result.call_id.as_str() == "slow-call"
                && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "slow-call"
                && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResultDisplay(result)
            if result.call_id.as_str() == "slow-call"
                && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
    )));
    let placeholder_positions = [
        event_log_position(&h, |event| {
            matches!(
                event,
                Event::ProviderToolResult(result)
                    if result.call_id.as_str() == "slow-call"
                        && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
            )
        }),
        event_log_position(&h, |event| {
            matches!(
                event,
                Event::ToolResult(result)
                    if result.call_id.as_str() == "slow-call"
                        && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
            )
        }),
        event_log_position(&h, |event| {
            matches!(
                event,
                Event::ToolResultDisplay(result)
                    if result.call_id.as_str() == "slow-call"
                        && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
            )
        }),
    ]
    .map(|position| position.expect("placeholder event"));
    assert!(placeholder_positions[0] < placeholder_positions[1]);
    assert!(placeholder_positions[1] < placeholder_positions[2]);

    let followup_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side follow-up prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: followup_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "side answer".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("core-subagents"),
            query_id: "q-bg".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish side conversation");
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&side_cid),
        "tool-backed delegate conversation is detached/preserved after completion"
    );
    assert_eq!(
        h.tool_routing.tool_runtime.tool_agents.get("slow-call"),
        Some(&side_cid)
    );

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-slow"),
        Event::ToolResultReported(ToolResult {
            presentation: Default::default(),
            call_id: "slow-call".into(),
            tool_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("real output".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
    )
    .expect("late tool result");

    assert!(event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ToolBackgroundResult(result)
                if result.call_id.as_str() == "slow-call"
                    && matches!(&result.result, CborValue::Text(text) if text == "real output")
        )
    ));
    let canonical_position = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolBackgroundResult(result) if result.call_id.as_str() == "slow-call"
        )
    })
    .expect("canonical background result");
    let display_position = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolBackgroundResultDisplay(result)
                if result.call_id.as_str() == "slow-call"
        )
    })
    .expect("background display projection");
    assert!(canonical_position < display_position);
    let parent = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&parent_cid)
        .expect("parent conversation remains live");
    assert!(
        parent
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&"slow-call".into()))
    );
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .background_completion_targets
            .get("slow-call"),
        Some(&side_cid),
        "late background completions stay routed to the preserved delegate"
    );

    h.shutdown().expect("shutdown");
}

/// An active tool round folds the real background completion notice into a
/// typed steering fact rather than relying on text classification in the UI.
#[test]
fn active_background_completion_steering_carries_typed_provenance() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id = ToolCallId::from("typed-active-completion");
    h.set_agent_turn_state(
        &cid,
        AgentTurnState::ToolsRunning {
            remaining_calls: vec![call_id.clone()],
        },
    );

    h.queue_background_completion_prompt_without_advancing(&cid, &call_id);
    h.maybe_complete_agent_turn_for(&cid, call_id.as_str());

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.text == background_completion_prompt(&call_id)
                && steered.message_class == tau_proto::PromptMessageClass::Internal
                && steered.internal_kind
                    == Some(tau_proto::InternalPromptKind::BackgroundToolCompletion)
    )));
    h.shutdown().expect("shutdown");
}

/// Regression: background errors clear the same actual-running state as
/// background results, without affecting unrelated tool calls that already
/// dispatched.
#[test]
fn background_error_clears_actual_running_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let tool_events = connect_ready_configured_extension(
        &mut h,
        "conn-bg-error-drain",
        "configured-conn-bg-error-drain",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-error-drain"),
        scheduled_test_tool_spec("bg_exclusive", tau_proto::BackgroundSupport::Instant),
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-error-drain"),
        scheduled_test_tool_spec(
            "queued_update_after_error",
            tau_proto::BackgroundSupport::Never,
        ),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-bg-error-drain");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "bg-exclusive-running".into(),
                name: ToolName::new("bg_exclusive"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-update-after-error".into(),
                name: ToolName::new("queued_update_after_error"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
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
    .expect("tool response");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec![
            "bg-exclusive-running".to_owned(),
            "queued-update-after-error".to_owned(),
        ]
    );
    assert_eq!(background_placeholder_count(&h, "bg-exclusive-running"), 1);
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&"bg-exclusive-running".into())
    );
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-bg-error-drain"),
        Event::ToolErrorReported(tool_error(
            "bg-exclusive-running",
            "bg_exclusive",
            "background failure",
        )),
    )
    .expect("background error accepted");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec![
            "bg-exclusive-running".to_owned(),
            "queued-update-after-error".to_owned(),
        ]
    );
    assert_eq!(background_error_count(&h, "bg-exclusive-running"), 1);
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&"bg-exclusive-running".into())
    );
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("bg-exclusive-running")
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("queued-update-after-error")
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a cancelled backgrounded call clears actual-running state
/// and publishes a background error instead of an invalid terminal
/// cancellation.
#[test]
fn background_cancel_clears_actual_running_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let tool_events = connect_ready_configured_extension(
        &mut h,
        "conn-bg-cancel-drain",
        "configured-conn-bg-cancel-drain",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-cancel-drain"),
        scheduled_test_tool_spec("bg_exclusive_cancel", tau_proto::BackgroundSupport::Instant),
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-cancel-drain"),
        scheduled_test_tool_spec(
            "queued_update_after_cancel",
            tau_proto::BackgroundSupport::Never,
        ),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-bg-cancel-drain");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "bg-exclusive-cancel-running".into(),
                name: ToolName::new("bg_exclusive_cancel"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-update-after-cancel".into(),
                name: ToolName::new("queued_update_after_cancel"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
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
    .expect("tool response");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec![
            "bg-exclusive-cancel-running".to_owned(),
            "queued-update-after-cancel".to_owned(),
        ]
    );
    assert_eq!(
        background_placeholder_count(&h, "bg-exclusive-cancel-running"),
        1
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&"bg-exclusive-cancel-running".into())
    );
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-bg-cancel-drain"),
        Event::ToolCancelledReported(tau_proto::ToolCancelled {
            presentation: Default::default(),
            call_id: "bg-exclusive-cancel-running".into(),
            tool_name: ToolName::new("bg_exclusive_cancel"),
            tool_type: tau_proto::ToolType::Function,
            display: None,
        }),
    )
    .expect("background cancellation accepted");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec![
            "bg-exclusive-cancel-running".to_owned(),
            "queued-update-after-cancel".to_owned(),
        ]
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&"bg-exclusive-cancel-running".into())
    );
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("bg-exclusive-cancel-running")
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("queued-update-after-cancel")
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled)
            if cancelled.call_id.as_str() == "bg-exclusive-cancel-running"
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "bg-exclusive-cancel-running"
                && error.message == "Tool cancelled"
    )));
    assert!(
        h.tool_routing
            .tool_runtime
            .background_completion_targets
            .contains_key("bg-exclusive-cancel-running")
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundResult(result)
            if result.call_id.as_str() == "bg-exclusive-cancel-running"
    )));

    h.shutdown().expect("shutdown");
}

/// A committed background placeholder closes the foreground round even though
/// the real call remains live; stale runtime-only call entries must not strand
/// that continuation or duplicate the canonical placeholder.
#[test]
fn background_placeholder_repairs_stale_foreground_projection() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _tool = connect_test_tool(&mut h, "background-repair-tool");
    h.tool_routing.registry.register(
        &crate::test_connection_id("background-repair-tool"),
        scheduled_test_tool_spec("background_repair", tau_proto::BackgroundSupport::Never),
    );
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::user("start background repair".to_owned()),
    )
    .expect("dispatch prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(provider_tool_response(
        &prompt,
        "background-repair-call",
        "background_repair",
        CborValue::Map(Vec::new()),
    ))
    .expect("dispatch tool");
    let call_id: ToolCallId = "background-repair-call".into();
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .begin_backgrounding(&call_id)
    );
    h.observe_tool_backgrounded(&call_id);
    let AgentTurnState::ToolsRunning { remaining_calls } = &mut h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .turn_state
    else {
        panic!("tool round must be running");
    };
    remaining_calls.push("stale-background-projection".into());

    h.publish_synthetic_background_result(&call_id);

    assert_eq!(background_placeholder_count(&h, call_id.as_str()), 1);
    assert_eq!(
        event_log_count(&h, |event| matches!(event, Event::AgentPromptCreated(_))),
        2,
        "placeholder must start exactly one continuation"
    );
    let successor = read_nth_prompt_created(&h, 1);
    assert_eq!(
        agent_event_count(&h, |event| matches!(
            event,
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == successor.agent_prompt_id
        )),
        1,
        "placeholder repair must commit one durable successor"
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// Regression for tau-agent-95m: the harness host API used by the built-in
/// `cancel` tool must only accept target calls owned by the requesting
/// conversation. Otherwise one agent that learns another agent's call id can
/// broadcast a cancellation request for unrelated work.
#[test]
fn cancel_request_api_rejects_non_owner_conversation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let owner = AgentId::parse("owner").expect("owner id");
    let attacker = AgentId::parse("attacker").expect("attacker id");
    let target: ToolCallId = "owned-running-call".into();

    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(target.clone(), owner.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        target.clone(),
        PendingTool {
            name: ToolName::new("slow_tool"),
            internal_name: ToolName::new("slow_tool"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );

    assert!(!h.is_running_cancellable_tool_call_for(&attacker, &target));
    assert_eq!(
        h.publish_tool_cancel_request_for(&attacker, None, target.clone())
            .expect_err("non-owner must not cancel the call"),
        "Unknown tool call id"
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelRequest(request) if request.target_call_id == target
    )));

    assert!(h.is_running_cancellable_tool_call_for(&owner, &target));
    h.publish_tool_cancel_request_for(&owner, None, target.clone())
        .expect("owner may cancel the call");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelRequest(request) if request.target_call_id == target
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: harness-authored turn cancellation must not publish a second
/// transcript-terminal `ToolCancelled` after a background placeholder has
/// already closed the foreground tool round. Backgrounded calls are completed
/// through the durable background channel instead.
#[test]
fn cancel_remaining_backgrounded_extension_call_publishes_background_error_only() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-cancel-bg");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-cancel-bg"),
        instant_background_test_tool_spec("cancel_bg_tool"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-cancel-bg-tool");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    let target_agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.prompt_coordination
        .prompt_runtime
        .estimated_cost_rates
        .insert(spid.clone(), tau_proto::ESTIMATED_API_COST_FALLBACK);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&target_agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "cancel-bg-call".into(),
            name: ToolName::new("cancel_bg_tool"),
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
        provider_attempt: Default::default(),
        usage: Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens: 1_000_000,
            ..tau_proto::ProviderTokenUsage::default()
        }),
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool call routed");

    let call_id: ToolCallId = "cancel-bg-call".into();
    assert_eq!(background_placeholder_count(&h, call_id.as_str()), 1);
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&call_id)
    );

    h.cancel_remaining_tool_calls(
        &cid,
        vec![call_id.clone()],
        BackgroundCompletionPromptMode::QueuePassive,
    );

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelRequest(request) if request.target_call_id == call_id
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id == call_id
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id == call_id && error.message == "Tool call canceled"
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice) if notice.kind == tau_proto::notice_kind::HARNESS_FAILURE
    )));
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&call_id)
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .any(|prompt| {
                prompt.text == background_completion_prompt(&call_id) && prompt.is_internal()
            }),
        "live-branch cancellation should leave a queued internal completion notice"
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a passive background notice left on the canceled live branch
/// must not block ordinary queued work for other agents. Once passive-only
/// queues are ignored by `next_runnable_agent`, live cancellation should still
/// advance the global queue so another agent's normal prompt can run.
#[test]
fn live_cancel_passive_notice_still_advances_other_runnable_agent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let cancel_cid = ensure_test_user_agent(&mut h);
    publish_test_tool_declaration(&mut h, &cancel_cid, "live-cancel-bg-with-other-agent");
    let cancel_agent_id = h.agent_runtime.agent_registry.agents[&cancel_cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    let other_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&other_cid)
        .expect("other conversation")
        .dispatch
        .pending_prompts
        .push_back(PendingPrompt::user("other agent prompt".to_owned()));

    let call_id: ToolCallId = "live-cancel-bg-with-other-agent".into();
    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("live_cancel_bg_tool"),
            internal_name: ToolName::new("live_cancel_bg_tool"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), cancel_cid.clone());
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(
            cancel_cid.clone(),
            call_id.clone(),
            ToolTurnCategories::default(),
        );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .begin_backgrounding(&call_id)
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .mark_backgrounded(&call_id)
    );
    h.publish_synthetic_background_result(&call_id);
    seed_tools_running(&mut h, &cancel_cid, vec![call_id.clone()]);

    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(crate::parse_agent_id(&cancel_agent_id)),
            agent_prompt_id: None,
        },
    );

    assert!(
        h.agent_runtime.agent_registry.agents[&cancel_cid]
            .dispatch
            .pending_prompts
            .iter()
            .any(|prompt| {
                prompt.text == background_completion_prompt(&call_id)
                    && prompt.is_passive_background_completion()
            }),
        "canceled agent should keep only a passive completion notice"
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text == background_completion_prompt(&call_id)
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == "other agent prompt"
    )));
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&other_cid]
            .turn
            .turn_state,
        AgentTurnState::AgentThinking { .. }
    ));

    h.shutdown().expect("shutdown");
}

/// Regression: a harness-owned, backgrounded `agent_start` must be completed
/// through the background-error channel when delegate teardown cancels it. A
/// second transcript-terminal `ToolCancelled` would make the store reject the
/// already-closed background placeholder branch.
#[test]
fn cancel_backgrounded_builtin_agent_start_publishes_background_error_only() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    h.install_internal_tool_handlers(vec![std::sync::Arc::new(TestAgentStartBuiltin)]);

    let parent_cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-builtin-agent-start-bg");
    seed_agent_thinking(&mut h, &parent_cid, spid.as_str());
    let parent_agent_id = h.agent_runtime.agent_registry.agents[&parent_cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), parent_cid.clone());
    let call_id: ToolCallId = "builtin-agent-start-bg".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&parent_agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.clone(),
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
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("builtin agent_start dispatched");
    assert_eq!(background_placeholder_count(&h, call_id.as_str()), 1);
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&call_id)
    );

    let query_id = format!("test-agent-start-{call_id}");
    let side_cid = ext_query_cid(&h, &query_id).expect("side conversation");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&side_cid)
            .and_then(|agent| agent.identity.parent_tool_call_id.as_ref()),
        Some(&call_id)
    );

    h.cancel_remaining_tool_calls(
        &parent_cid,
        vec![call_id.clone()],
        BackgroundCompletionPromptMode::DoNotQueue,
    );

    assert_eq!(background_error_count(&h, call_id.as_str()), 1);
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::ToolCancelled(cancelled) if cancelled.call_id == call_id
        )),
        0
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice) if notice.kind == tau_proto::notice_kind::HARNESS_FAILURE
    )));
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tools
            .contains_key(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&call_id)
    );

    h.shutdown().expect("shutdown");
}

/// Regression: live turn cancellation of backgrounded builtin `agent_start`
/// must still leave a passive completion notice even though the builtin cancel
/// handler suppresses normal background prompts while synchronously resolving
/// the `StartAgentResult`.
#[test]
fn live_cancel_backgrounded_builtin_agent_start_keeps_passive_completion_notice() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    h.install_internal_tool_handlers(vec![std::sync::Arc::new(TestAgentStartBuiltin)]);

    let parent_cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-live-builtin-agent-start-bg");
    seed_agent_thinking(&mut h, &parent_cid, spid.as_str());
    let parent_agent_id = h.agent_runtime.agent_registry.agents[&parent_cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), parent_cid.clone());
    let call_id: ToolCallId = "live-builtin-agent-start-bg".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&parent_agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.clone(),
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
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("builtin agent_start dispatched");
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&call_id)
    );

    seed_tools_running(&mut h, &parent_cid, vec![call_id.clone()]);
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(crate::parse_agent_id(&parent_agent_id)),
            agent_prompt_id: None,
        },
    );

    assert_eq!(background_error_count(&h, call_id.as_str()), 1);
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id == call_id
    )));
    assert!(
        h.agent_runtime.agent_registry.agents[&parent_cid]
            .dispatch
            .pending_prompts
            .iter()
            .any(|prompt| {
                prompt.text == background_completion_prompt(&call_id)
                    && prompt.is_passive_background_completion()
            }),
        "live builtin cancellation should not lose the passive completion notice"
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .suppressed_background_completion_prompts
            .contains(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tools
            .contains_key(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&call_id)
    );

    h.shutdown().expect("shutdown");
}

/// Regression: cancellation routing must re-check background state after the
/// cancel request has been published. A future synchronous cancel-request
/// handler may turn a still-foreground call into a backgrounded call without
/// completing it; that must be finalized through `ToolBackgroundError`, not a
/// second foreground `ToolCancelled`.
#[test]
fn cancel_target_rechecks_background_state_after_cancel_request() {
    let (_td, mut h) = setup_routed_test_tool_call("post-request-bg-call", "post_request_bg_tool");
    let call_id: ToolCallId = "post-request-bg-call".into();
    let target = CancelTarget {
        call_id: call_id.clone(),
        tool_name: ToolName::new("post_request_bg_tool"),
        tool_type: tau_proto::ToolType::Function,
        backgrounded: false,
    };

    assert!(!h.cancel_target_should_finish_as_background_error(&target));
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::ToolCancelRequest(tau_proto::ToolCancelRequest {
            target_call_id: call_id.clone(),
        }),
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .begin_backgrounding(&call_id)
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .mark_backgrounded(&call_id)
    );
    h.publish_synthetic_background_result(&call_id);

    assert!(h.cancel_target_should_finish_as_background_error(&target));
    h.finish_backgrounded_tool_cancelled_by_harness(
        target,
        BackgroundCompletionPromptMode::QueuePassive,
    );

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id == call_id
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id == call_id && error.message == "Tool call canceled"
    )));
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&call_id)
    );

    h.shutdown().expect("shutdown");
}

/// Cancelling a turn while `wait` is blocked must remove the waiter entry. A
/// later wait for the same target should report the cancelled/consumed target,
/// not a stale "existing wait" from the aborted wait call.
#[test]
fn cancel_clears_active_wait_state() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let target_call_id: ToolCallId = "wait-target".into();
    let wait_call_id: ToolCallId = "wait-call".into();

    let target_agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    seed_assistant_tool_round(
        &mut h,
        &cid,
        &[
            (target_call_id.as_str(), "slow"),
            (wait_call_id.as_str(), "wait"),
        ],
    );
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(target_call_id.clone(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        target_call_id.clone(),
        PendingTool {
            name: ToolName::new("slow"),
            internal_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.record_wait_tool_request(&target_call_id);

    let wait_call = AgentToolCall {
        call_ref: None,
        id: wait_call_id.clone(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(target_call_id.to_string()),
        )]),
    };
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("start wait");
    seed_tools_running(
        &mut h,
        &cid,
        vec![target_call_id.clone(), wait_call_id.clone()],
    );
    let _interceptor = connect_test_tool(&mut h, "user-cancel-terminal-interceptor");
    h.handle_extension_event(
        "user-cancel-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_CANCELLED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register cancellation interceptor");

    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
            agent_prompt_id: None,
        },
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_cancel
            .is_some()
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&target_call_id)
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&wait_call_id)
    );
    h.handle_extension_event(
        "user-cancel-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit first cancellation");
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_cancel
            .is_some()
    );
    h.handle_extension_event(
        "user-cancel-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit final cancellation");
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_cancel
            .is_none()
    );

    let second_wait_call = AgentToolCall {
        call_ref: None,
        id: "wait-call-2".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(target_call_id.to_string()),
        )]),
    };
    h.handle_wait_tool_call(&cid, &second_wait_call, ToolName::new("wait"))
        .expect("second wait");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-call-2"
                && error.message.contains("already consumed")
    )));

    h.shutdown().expect("shutdown");
}

/// Cancellation releases only the checkpoint owned by the canceled prompt id;
/// an unrelated ordinary-inference checkpoint must remain fail-closed.
#[test]
fn cancel_while_thinking_keeps_mismatched_inference_dispatch_ownership() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    let canceled_id: AgentPromptId = test_agent_prompt_id("sp-cancel-mismatched-thinking");
    let owned_id: AgentPromptId = test_agent_prompt_id("sp-owned-by-other-dispatch");
    seed_agent_thinking(&mut h, &cid, canceled_id.as_str());
    let target_agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    let agent = h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("conversation");
    agent.dispatch.in_flight_prompt = Some(canceled_id.clone());
    agent.dispatch.activation_dispatch =
        path_crate_agent::ActivationDispatchState::DispatchUncertain {
            owner: path_crate_agent::InferenceCheckpointOwner::Inference,
            agent_prompt_id: owned_id.clone(),
            through: tau_proto::AgentHead::Root,
            model: Some("test/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
        };
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(canceled_id.clone(), cid.clone());

    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
            agent_prompt_id: Some(canceled_id),
        },
    );

    assert!(matches!(
        &h.agent_runtime.agent_registry.agents[&cid].dispatch.activation_dispatch,
        crate::agent::ActivationDispatchState::DispatchUncertain {
            owner: crate::agent::InferenceCheckpointOwner::Inference,
            agent_prompt_id,
            ..
        } if *agent_prompt_id == owned_id
    ));

    h.shutdown().expect("shutdown");
}

/// Canceled side agents must not transfer their inner background tools
/// to the parent. Otherwise a canceled delegate can leak an inner shell
/// completion prompt and make that inner call waitable in the parent
/// conversation.
#[test]
fn canceled_side_conversation_drops_inner_background_completion() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: ToolName::new("agent_start"),
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
    let _ = connect_ready_configured_extension(
        &mut h,
        "conn-slow",
        "configured-conn-slow",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-slow"),
        ToolSpec {
            name: ToolName::new("slow"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
            examples: Vec::new(),
        },
    );

    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-slow"),
        scheduled_test_tool_spec("foreground_slow", tau_proto::BackgroundSupport::Never),
    );
    let parent_cid = ensure_test_user_agent(&mut h);
    let parent_agent_id = h.agent_runtime.agent_registry.agents[&parent_cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    let main_spid: AgentPromptId = test_agent_prompt_id("sp-main-cancel");
    seed_agent_thinking(&mut h, &parent_cid, "sp-main-cancel");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(main_spid.clone(), parent_cid.clone());
    h.publish_for_agent(
        &parent_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "delegate slow work".to_owned(),
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
        agent_id: crate::parse_agent_id(&parent_agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call-cancel".into(),
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
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main delegate call");

    let mut query = ext_query("q-bg-cancel");
    query.tool_call_id = Some("delegate-call-cancel".into());
    h.handle_start_agent_request(&crate::test_connection_id("conn-delegate"), query)
        .expect("side query");
    let side_cid = ext_query_cid(&h, "q-bg-cancel").expect("side conversation");
    let side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    let side_agent_id = h.agent_runtime.agent_registry.agents[&side_cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: side_spid,
        agent_id: crate::parse_agent_id(&side_agent_id),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "slow-call-cancel".into(),
                name: ToolName::new("slow"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "foreground-slow-call-cancel".into(),
                name: ToolName::new("foreground_slow"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("core-subagents"),
            query_id: "q-bg-cancel".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side tool call");
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&ToolCallId::from("slow-call-cancel")),
        "scheduler placeholder must commit before delegate cancellation"
    );
    let _interceptor = connect_test_tool(&mut h, "delegate-cancel-terminal-interceptor");
    h.handle_extension_event(
        "delegate-cancel-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_CANCELLED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register delegate cancellation interceptor");

    h.cancel_start_agent_request("q-bg-cancel", &"delegate-call-cancel".into(), false)
        .expect("cancel delegate");
    assert!(
        h.agent_runtime.agent_registry.agents[&side_cid]
            .dispatch
            .terminating
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("foreground-slow-call-cancel")
    );
    h.handle_extension_event(
        "delegate-cancel-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit delegate cancellation");
    assert!(
        !h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&side_cid)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("slow-call-cancel")
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "slow-call-cancel"
                && error.message == "Tool call canceled"
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "slow-call-cancel"
    )));
    assert!(
        !h.tool_routing
            .tool_runtime
            .background_completion_targets
            .contains_key("slow-call-cancel")
    );

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-slow"),
        Event::ToolResultReported(ToolResult {
            presentation: Default::default(),
            call_id: "slow-call-cancel".into(),
            tool_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("real output".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
    )
    .expect("late tool result is ignored");

    assert!(!event_log_contains(&h, "conn-slow", |event| matches!(
        event,
        Event::ToolBackgroundResult(result) if result.call_id.as_str() == "slow-call-cancel"
    )));
    let parent = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&parent_cid)
        .expect("parent conversation remains live");
    assert!(
        !parent
            .dispatch
            .pending_prompts
            .iter()
            .any(
                |prompt| prompt.text == background_completion_prompt(&"slow-call-cancel".into())
                    && prompt.is_internal()
            )
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn wait_returns_internal_background_error_after_extension_disconnect() {
    // A backgrounded call belongs to its call id, not to a future provider
    // registration. When the extension disconnects, `wait` must consume the
    // synthesized background error immediately instead of hanging.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-bg-disconnect");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-disconnect"),
        instant_background_test_tool_spec("slow_disconnect"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-disconnect".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        call_id.as_str(),
        "slow_disconnect",
    );
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .get(&call_id)
            .map(|provider| provider.as_str()),
        Some("conn-bg-disconnect")
    );

    h.handle_disconnect(&crate::test_connection_id("conn-bg-disconnect"));

    let expected = extension_disconnected_background_tool_call_error_message(&call_id);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == call_id.as_str()
                && error.message == expected
    )));

    let _replacement_events = connect_test_tool(&mut h, "conn-bg-replacement");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-bg-replacement"),
        instant_background_test_tool_spec("slow_disconnect"),
    );

    let wait_call = AgentToolCall {
        call_ref: None,
        id: "wait-bg-disconnect".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(call_id.to_string()),
        )]),
    };
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("wait returns disconnected background error");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-bg-disconnect"
                && error.message == expected
    )));

    h.shutdown().expect("shutdown");
}

/// Rejected bare/background waits are not activating-input timeouts and cannot
/// advance the repeated-timeout advisory counter.
#[test]
fn background_wait_rejections_do_not_count_as_input_timeouts() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let background_id = ToolCallId::from("completed-background-before-timeout");
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(background_id.clone(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        background_id.clone(),
        PendingTool {
            name: ToolName::new("slow"),
            internal_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.record_wait_tool_request(&background_id);
    h.record_wait_tool_result(
        &ToolResult {
            presentation: Default::default(),
            call_id: background_id.clone(),
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
    h.record_wait_background_result(
        tau_proto::ToolBackgroundResult {
            call_id: background_id,
            tool_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("done".to_owned()),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        },
        Some(tau_proto::ObservationId::random()),
    );
    let mut successful_background_wait = wait_no_args_call("successful-background-wait");
    successful_background_wait.call_ref = Some(tau_proto::ToolCallRef {
        declaration: tau_proto::ObservationId::from_bytes([1; 16]),
        item_index: 0,
    });
    seed_tools_running(&mut h, &cid, vec![successful_background_wait.id.clone()]);
    h.handle_wait_tool_call(&cid, &successful_background_wait, ToolName::new("wait"))
        .expect("consume completed background result");
    for call_id in ["bare-wait-a", "bare-wait-b"] {
        let call = wait_no_args_call(call_id);
        seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
        h.handle_wait_tool_call(&cid, &call, ToolName::new("wait"))
            .expect("reject bare wait without background work");
    }
    let mut call = wait_input_call("first-real-timeout");
    call.call_ref = Some(tau_proto::ToolCallRef {
        declaration: tau_proto::ObservationId::from_bytes([3; 16]),
        item_index: 0,
    });
    seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
    h.handle_wait_tool_call(&cid, &call, ToolName::new("wait"))
        .expect("register input wait");
    h.process_input_wait_deadlines(h.next_input_wait_deadline().expect("input deadline"));
    assert!(event_log_contains_any_source(&h, |event| {
        matches!(
            event,
            Event::ToolResult(result)
                if result.call_id == call.id
                    && matches!(&result.result, CborValue::Map(entries)
                        if entries.len() == 1
                            && entries[0].0 == CborValue::Text("timed_out".to_owned()))
        )
    }));
    h.shutdown().expect("shutdown");
}

/// The combined runtime scheduler chooses the earlier background or input
/// deadline in both directions, then advances to the remaining deadline.
#[test]
fn runtime_deadline_scheduler_orders_input_and_background_deadlines() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let input = wait_input_call("wait-input-combined-deadline");
    seed_assistant_tool_round(
        &mut h,
        &cid,
        &[
            (input.id.as_str(), "wait"),
            ("combined-deadline-sibling", "slow"),
            ("background-deadline-first", "slow"),
            ("background-deadline-after-input", "slow"),
        ],
    );
    seed_tools_running(
        &mut h,
        &cid,
        vec![
            input.id.clone(),
            ToolCallId::from("combined-deadline-sibling"),
            ToolCallId::from("background-deadline-first"),
            ToolCallId::from("background-deadline-after-input"),
        ],
    );
    for call_id in [
        ToolCallId::from("background-deadline-first"),
        ToolCallId::from("background-deadline-after-input"),
    ] {
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert(call_id.clone(), cid.clone());
        h.tool_routing.tool_runtime.pending_tools.insert(
            call_id,
            PendingTool {
                name: ToolName::new("slow"),
                internal_name: ToolName::new("slow"),
                tool_type: tau_proto::ToolType::Function,
                allows_provider_image: false,
            },
        );
    }
    h.handle_wait_tool_call(&cid, &input, ToolName::new("wait"))
        .expect("register input wait");
    let input_deadline = h.next_input_wait_deadline().expect("input deadline");

    let background_call = AgentToolCall {
        call_ref: None,
        id: "background-deadline-first".into(),
        name: ToolName::new("slow"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    };
    h.tool_routing.tool_runtime.tool_turn.push(
        cid.clone(),
        background_call.clone(),
        tau_proto::BackgroundSupport::MinForegroundSeconds(30),
    );
    let background_start = input_deadline - Duration::from_secs(60);
    h.tool_routing
        .tool_runtime
        .tool_turn
        .pop_dispatchable(background_start)
        .expect("dispatch backgroundable call");
    let background_deadline = background_start + Duration::from_secs(30);
    assert_eq!(h.next_runtime_deadline(), Some(background_deadline));

    h.process_runtime_deadlines_at(background_deadline);
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&background_call.id)
    );

    let later_background_call = AgentToolCall {
        call_ref: None,
        id: "background-deadline-after-input".into(),
        name: ToolName::new("slow"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    };
    h.tool_routing.tool_runtime.tool_turn.push(
        cid.clone(),
        later_background_call.clone(),
        tau_proto::BackgroundSupport::MinForegroundSeconds(60),
    );
    h.tool_routing
        .tool_runtime
        .tool_turn
        .pop_dispatchable(input_deadline - Duration::from_secs(30))
        .expect("dispatch later backgroundable call");
    let later_background_deadline = input_deadline + Duration::from_secs(30);
    assert_eq!(h.next_runtime_deadline(), Some(input_deadline));
    h.process_runtime_deadlines_at(input_deadline);
    assert_eq!(tool_result_count(&h, input.id.as_str()), 1);
    assert_eq!(h.next_runtime_deadline(), Some(later_background_deadline));
    h.process_runtime_deadlines_at(later_background_deadline);
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&later_background_call.id)
    );
    h.shutdown().expect("shutdown");
}

/// Passive completion/restore context is deliberately not an activation and
/// cannot release an input wait until a separate activating prompt arrives.
#[test]
fn passive_background_notice_does_not_wake_input_wait() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call = wait_input_call("wait-input-passive");
    seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
    h.handle_wait_tool_call(&cid, &call, ToolName::new("wait"))
        .expect("register input wait");

    let passive_call: ToolCallId = "passive-input-wait-notice".into();
    h.tool_routing
        .tool_runtime
        .background_completion_targets
        .insert(passive_call.clone(), cid.clone());
    h.queue_passive_background_completion_prompt(&cid, &passive_call);
    assert_eq!(tool_result_count(&h, call.id.as_str()), 0);

    let durable_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.submit_prompt_to_agent(
        h.session_runtime.current_session_id.clone(),
        &durable_id,
        PendingPrompt::internal("live timer".to_owned()),
    )
    .expect("queue activation");
    assert_eq!(tool_result_count(&h, call.id.as_str()), 1);
    h.shutdown().expect("shutdown");
}

/// An unsuppressed activating background-completion notice may wake an input
/// wait, but the completion itself remains unconsumed for a later bare wait.
#[test]
fn background_completion_wakes_input_wait_without_consuming_result() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let input_call = wait_input_call("wait-input-background");
    seed_tools_running(&mut h, &cid, vec![input_call.id.clone()]);
    h.handle_wait_tool_call(&cid, &input_call, ToolName::new("wait"))
        .expect("register input wait");

    let background_id: ToolCallId = "background-for-input".into();
    h.tool_routing
        .tool_runtime
        .background_completion_targets
        .insert(background_id.clone(), cid.clone());
    h.record_wait_background_result(
        tau_proto::ToolBackgroundResult {
            call_id: background_id.clone(),
            tool_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("background payload".to_owned()),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        },
        Some(tau_proto::ObservationId::random()),
    );
    h.queue_background_completion_prompt_without_advancing(&cid, &background_id);
    assert_eq!(tool_result_count(&h, input_call.id.as_str()), 1);

    let consume_call = wait_no_args_call("wait-consume-after-input");
    h.handle_wait_tool_call(&cid, &consume_call, ToolName::new("wait"))
        .expect("consume background result");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-consume-after-input"
                && cbor_map_text(&result.result, "original_tool_call_id")
                    == Some(background_id.as_str())
    )));
    h.shutdown().expect("shutdown");
}

/// Unloading a side agent must commit cancellation before unload, retire its
/// routing, waits, and completion prompts without transferring them, and ignore
/// late reports without injecting any completion into another agent. A failed
/// child terminal append retains unresolved ownership and runtime routing.
#[test]
fn background_completion_from_removed_side_conversation_is_retired() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-agent");
    let _ = connect_ready_configured_extension(
        &mut h,
        "conn-slow",
        "configured-conn-slow",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-slow"),
        ToolSpec {
            name: ToolName::new("slow"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
            examples: Vec::new(),
        },
    );
    let parent_cid = ensure_test_user_agent(&mut h);
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-agent"),
        ext_query("q-removed-bg"),
    )
    .expect("side query");
    let side_cid = ext_query_cid(&h, "q-removed-bg").expect("side conversation");
    let side_agent_id = h.agent_runtime.agent_registry.agents[&side_cid]
        .identity
        .agent_id
        .clone()
        .expect("side agent id");
    let call_id: ToolCallId = "removed-slow-call".into();

    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("slow"),
            internal_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), side_cid.clone());
    h.tool_routing
        .tool_runtime
        .background_completion_targets
        .insert(call_id.clone(), side_cid.clone());
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(
            side_cid.clone(),
            call_id.clone(),
            ToolTurnCategories::default(),
        );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .begin_backgrounding(&call_id)
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .mark_backgrounded(&call_id)
    );
    h.queue_background_completion_prompt(&side_cid, &call_id);

    h.remove_agent(&side_cid);

    assert!(
        !h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&side_cid)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .background_completion_targets
            .contains_key(&call_id)
    );
    let parent = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&parent_cid)
        .expect("parent conversation remains live");
    assert!(
        parent
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );
    assert!(event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ToolCancelRequest(request) if request.target_call_id == call_id
        )
    ));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id == call_id && error.message == "Tool call canceled"
    )));
    assert!(
        event_log_position(&h, |event| {
            matches!(
                event,
                Event::SessionAgentUnloaded(unloaded)
                    if unloaded.agent_id.as_str() == side_agent_id
            )
        })
        .is_some()
    );

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-slow"),
        Event::ToolResultReported(ToolResult {
            presentation: Default::default(),
            call_id: call_id.clone(),
            tool_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("late".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,
            display: None,
        }),
    )
    .expect("late report is ignored");
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundResult(result) if result.call_id == call_id
    )));
    let parent = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&parent_cid)
        .expect("parent conversation remains live");
    assert!(
        parent
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-agent"),
        ext_query("q-removed-bg-fault"),
    )
    .expect("fault side query");
    let fault_cid = ext_query_cid(&h, "q-removed-bg-fault").expect("fault side conversation");
    let fault_agent_id = h.agent_runtime.agent_registry.agents[&fault_cid]
        .identity
        .agent_id
        .clone()
        .expect("fault side agent id");
    let fault_call_id: ToolCallId = "removed-slow-call-fault".into();
    h.tool_routing.tool_runtime.pending_tools.insert(
        fault_call_id.clone(),
        PendingTool {
            name: ToolName::new("slow"),
            internal_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(fault_call_id.clone(), fault_cid.clone());
    h.tool_routing
        .tool_runtime
        .background_completion_targets
        .insert(fault_call_id.clone(), fault_cid.clone());
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(
            fault_cid.clone(),
            fault_call_id.clone(),
            ToolTurnCategories::default(),
        );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .begin_backgrounding(&fault_call_id)
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .mark_backgrounded(&fault_call_id)
    );
    reject_semantic_admissions(&h, 2);

    h.remove_agent(&fault_cid);

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&fault_cid)
            .is_some_and(|agent| agent.dispatch.terminating)
    );
    assert_eq!(
        h.tool_routing.tool_runtime.tool_agents.get(&fault_call_id),
        Some(&fault_cid)
    );
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .background_completion_targets
            .get(&fault_call_id),
        Some(&fault_cid)
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::SessionAgentUnloaded(unloaded)
            if unloaded.agent_id.as_str() == fault_agent_id
    )));
    let parent = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&parent_cid)
        .expect("parent remains after failed child terminal");
    assert!(
        parent
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&fault_call_id))
    );
    h.shutdown().expect("shutdown");
}
/// Resume should treat existing background results/errors as terminal. They are
/// replayed into the wait tracker, but no restored interruption error is
/// appended over the real outcome.
#[test]
fn resume_keeps_existing_background_completions() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "finished-bg", "slow_bg");
    seed_background_placeholder(&sp, "failed-bg", "slow_bg");
    seed_background_result(&sp, "finished-bg", "slow_bg", "finished");
    seed_background_error(&sp, "failed-bg", "slow_bg", "real failure");

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");

    assert_eq!(background_result_count(&h, "finished-bg"), 1);
    assert_eq!(background_error_count(&h, "finished-bg"), 0);
    assert_eq!(background_error_count(&h, "failed-bg"), 1);
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.message == restored_background_notice(error.call_id.as_str())
    )));

    h.shutdown().expect("shutdown");
}

/// Completed background results restored from the agent log should be
/// available to `wait({})`, not only to exact-id waits.
#[test]
fn resumed_completed_background_result_can_be_consumed_by_no_arg_wait() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "restored-any", "slow_bg");
    seed_background_result(&sp, "restored-any", "slow_bg", "restored output");

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let cid = ensure_test_user_agent(&mut h);
    let source_call = h
        .persisted_tool_call_ref(&cid, &ToolCallId::from("restored-any"))
        .expect("restored declaration occurrence");
    let source_terminal = persisted_background_terminal(&h, &cid, "restored-any");
    assert_eq!(
        h.wait_tool_call_ref(&ToolCallId::from("restored-any")),
        Some(source_call)
    );
    assert_eq!(
        h.wait_tool_terminal_observation(&ToolCallId::from("restored-any")),
        Some(source_terminal)
    );
    h.submit_user_prompt(
        test_session_id("s1"),
        "collect restored background".to_owned(),
    )
    .expect("submit first resumed prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: prompt.agent_prompt_id,
        agent_id: prompt.agent_id,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "wait-restored-any".into(),
            name: ToolName::new("wait"),
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
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("wait on restored completion");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-restored-any"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some("restored-any")
                && cbor_map_text(&result.result, "output") == Some("restored output")
    )));

    h.shutdown().expect("shutdown");
}

/// Cold restore preserves exact source declaration and terminal identities.
#[test]
fn resumed_completed_background_result_preserves_exact_wait_correlation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "restored-exact", "slow_bg");
    seed_background_result(&sp, "restored-exact", "slow_bg", "restored output");

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let cid = ensure_test_user_agent(&mut h);
    let source_call = h
        .persisted_tool_call_ref(&cid, &ToolCallId::from("restored-exact"))
        .expect("restored declaration occurrence");
    let source_terminal = persisted_background_terminal(&h, &cid, "restored-exact");
    assert_eq!(
        h.wait_tool_call_ref(&ToolCallId::from("restored-exact")),
        Some(source_call)
    );
    assert_eq!(
        h.wait_tool_terminal_observation(&ToolCallId::from("restored-exact")),
        Some(source_terminal)
    );
    h.submit_user_prompt(
        test_session_id("s1"),
        "collect exact restored background".to_owned(),
    )
    .expect("submit resumed prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: prompt.agent_prompt_id,
        agent_id: prompt.agent_id,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "wait-restored-exact".into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("tool_call_id".into()),
                CborValue::Text("restored-exact".into()),
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
    .expect("exact wait on restored completion");

    assert_eq!(
        h.wait_tool_call_ref(&ToolCallId::from("restored-exact")),
        Some(source_call)
    );
    assert_eq!(
        h.wait_tool_terminal_observation(&ToolCallId::from("restored-exact")),
        Some(source_terminal)
    );
    h.shutdown().expect("shutdown");
}

/// The restored background error is durable. A later cold resume must observe
/// the existing error and avoid appending a duplicate.
#[test]
fn repeated_resume_does_not_duplicate_background_errors() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "lost-once", "slow_bg");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("first resume");
        assert_eq!(background_error_count(&h, "lost-once"), 1);
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("second resume");
        assert_eq!(background_error_count(&h, "lost-once"), 1);
        h.shutdown().expect("shutdown");
    }
}

/// A provider wait dispatched through the scheduler after its background target
/// completed must retain durable correlation without duplicating source output.
///
/// This protects `SPEC-durable-tool-observation-correlation`.
#[test]
fn scheduler_wait_for_completed_background_call_retains_durable_correlation() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = echo_harness(&state).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _tool_events = connect_ready_configured_extension(
        &mut h,
        "conn-completed-wait-correlation",
        "configured-conn-completed-wait-correlation",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-completed-wait-correlation"),
        instant_background_test_tool_spec("completed_wait_source"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let source_call_id = ToolCallId::from("completed-wait-source");
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        source_call_id.as_str(),
        "completed_wait_source",
    );
    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-completed-wait-correlation"),
        Event::ToolResultReported(final_tool_result(
            source_call_id.as_str(),
            "completed_wait_source",
            "source-owned output",
        )),
    )
    .expect("complete background source");

    let records = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent records");
    let source_terminal = records
        .iter()
        .find_map(|record| {
            matches!(&record.event, Event::ToolBackgroundResult(result)
                if result.call_id == source_call_id)
            .then_some(record.observation_id)
        })
        .expect("source terminal observation");
    let source_call = h
        .wait_tool_call_ref(&source_call_id)
        .expect("source call reference");

    let wait_call_id = ToolCallId::from("wait-for-completed-source");
    let completion_prompt = active_prompt_for(&h, &cid);
    let completion_prompt = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentPromptCreated(prompt) if prompt.agent_prompt_id == completion_prompt => {
                Some(prompt)
            }
            _ => None,
        })
        .expect("background completion prompt");
    h.handle_provider_response_finished(provider_tool_response(
        &completion_prompt,
        wait_call_id.as_str(),
        "wait",
        CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(source_call_id.to_string()),
        )]),
    ))
    .expect("dispatch provider wait");

    let wait_results = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolResult(result) if result.call_id == wait_call_id => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(wait_results.len(), 1);
    assert_eq!(
        wait_results[0].result,
        CborValue::Text("source-owned output".to_owned())
    );

    let records = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent records");
    let wait_call = records
        .iter()
        .find_map(|record| {
            match &record.event {
            Event::ProviderResponseFinished(response)
                if response.output_items.iter().any(|item| {
                    matches!(item, ContextItem::ToolCall(call) if call.call_id == wait_call_id)
                }) =>
            {
                Some(tau_proto::ToolCallRef {
                    declaration: record.observation_id,
                    item_index: 0,
                })
            }
            _ => None,
        }
        })
        .expect("persisted wait declaration");
    let observed = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentToolWaitObserved(observed) => Some((record.observation_id, observed)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(observed.len(), 1);
    let wait_observation = observed[0].0;
    assert_eq!(observed[0].1.wait_call, wait_call);
    assert_eq!(
        observed[0].1.mode,
        tau_proto::ToolWaitMode::Exact {
            target: source_call
        }
    );
    let settled = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentToolWaitSettled(settled) if settled.wait_call == wait_call => Some(settled),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(settled.len(), 1);
    let settled = settled[0];
    assert_eq!(settled.wait_observation, wait_observation);
    assert_eq!(settled.wait_call, wait_call);
    assert_eq!(settled.registration, None);
    assert!(matches!(
        settled.outcome,
        tau_proto::ToolWaitOutcome::CompletionDelivered {
            source_call: delivered_source,
            source_terminal: terminal,
            source_phase: tau_proto::ToolSourcePhase::Background,
            envelope: tau_proto::ToolOutputEnvelope::Identity,
        } if delivered_source == source_call && terminal == source_terminal
    ));

    h.shutdown().expect("flush accepted trace records");
    let mut trace = tau_session_inspect::prepare_agent_trace(
        &state.join("agents"),
        &crate::parse_agent_id(&agent_id),
        tau_session_inspect::DescendantSelection::RootOnly,
        tau_session_inspect::AgentTraceFormat::AgentToolsJsonl(
            tau_session_inspect::AgentTraceMode::Full,
        ),
    )
    .expect("prepare compact trace");
    let mut trace_bytes = Vec::new();
    trace.copy_to(&mut trace_bytes).expect("read compact trace");
    let trace_records = String::from_utf8(trace_bytes)
        .expect("UTF-8 trace")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace record"))
        .collect::<Vec<_>>();
    let wait_record = trace_records
        .iter()
        .find(|record| record["call_id"] == wait_call_id.as_str())
        .expect("wait call trace record");
    assert!(wait_record.get("output").is_none());
    assert!(wait_record.get("output_bytes").is_none());
    let settlement_record = trace_records
        .iter()
        .find(|record| {
            record["relationship"] == "wait_settlement"
                && record["wait_call"]["declaration"] == wait_call.declaration.to_string()
                && record["wait_call"]["item_index"] == wait_call.item_index
        })
        .expect("wait settlement trace record");
    assert_eq!(settlement_record["output_ref"], source_terminal.to_string());
}

/// Regression: restored background notices are owned by the agent whose
/// background call was repaired. A session-level queue let the first prompted
/// agent consume every restored background notice, including notices for other
/// loaded agents in the same session.
#[test]
fn restored_background_notices_are_delivered_to_owning_agent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder_for_agent(&sp, "main", "main-lost-bg", "slow_bg");
    seed_background_placeholder_for_agent(&sp, "side_agent", "side-lost-bg", "slow_bg");

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let main_notice = restored_background_notice("main-lost-bg");
    let side_notice = restored_background_notice("side-lost-bg");
    let main_cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get("main")
        .cloned()
        .expect("main agent route");
    let side_cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get("side_agent")
        .cloned()
        .expect("side agent route");

    let side_prompts = h.take_pending_restore_prompts_for_user_prompt(&side_cid);
    assert!(
        side_prompts.iter().any(|prompt| prompt.text == side_notice),
        "side agent should receive its own restored background notice"
    );
    assert!(
        side_prompts.iter().all(|prompt| prompt.text != main_notice),
        "side agent must not receive main agent's restored background notice"
    );

    let main_prompts = h.take_pending_restore_prompts_for_user_prompt(&main_cid);
    assert!(
        main_prompts.iter().any(|prompt| prompt.text == main_notice),
        "main agent should still receive its own restored background notice"
    );
    assert!(
        main_prompts.iter().all(|prompt| prompt.text != side_notice),
        "main agent must not receive side agent's restored background notice"
    );

    h.shutdown().expect("shutdown");
}
