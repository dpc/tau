//! Pure canonical-field preparation and effectful-wrapper equivalence.

use tau_config::settings::{CompactionPolicyThreshold, ContextPolicyPoint};

use super::*;
use crate::harness::AgentToolCall;
use crate::harness::terminal_response_projection::TerminalResponseProjection;

fn background_policy_tool(
    internal_name: &str,
    visible_name: &str,
    background_support: Option<tau_proto::BackgroundSupport>,
) -> ToolSpec {
    ToolSpec {
        provider_scope: None,
        name: ToolName::new(internal_name),
        model_visible_name: Some(ToolName::new(visible_name)),
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support,
        examples: Vec::new(),
    }
}

fn response_with_one_tool_call(
    source: &tau_proto::AgentPromptCreated,
    call_id: &str,
    name: &str,
) -> (ProviderResponseFinished, Vec<AgentToolCall>) {
    let call = AgentToolCall {
        call_ref: None,
        id: call_id.into(),
        name: ToolName::new(name),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    };
    let mut response = reasoning_only_length_response(source, 7);
    response.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
    response.output_items = vec![ContextItem::ToolCall(ToolCallItem {
        call_id: call.id.clone(),
        name: call.name.clone(),
        tool_type: call.tool_type,
        arguments: call.arguments.clone(),
        raw_arguments_json: None,
        responses_envelope: None,
    })];
    (response, vec![call])
}

/// The complete prepared ordinary payload must equal the eventual canonical
/// fact, without charging usage, consuming snapshots, or reserving a
/// continuation while its envelope is still being considered for admission.
#[test]
fn complete_ordinary_preparation_matches_canonical_publication_without_effects() {
    for stop in [
        tau_proto::ProviderStopReason::EndTurn,
        tau_proto::ProviderStopReason::Length,
    ] {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path()).expect("start");
        h.submit_user_prompt(test_session_id("s1"), "finish".to_owned())
            .expect("submit");
        let source = read_nth_prompt_created(&h, 0);
        let cid = h
            .agent_id_for_prompt(&source.agent_prompt_id)
            .expect("owner");
        let mut raw = reasoning_only_length_response(&source, 7);
        raw.stop_reason = stop;
        let mut candidate = raw.clone();
        let mut projection = TerminalResponseProjection::from_response(&candidate);
        let before_stats = h.session_runtime.current_session_state.token_usage.clone();
        let before_events = event_log_events(&h).len();
        let before_index = h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_prompt_index;
        let before_status = h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .terminal_status_was_available;
        let accounting = h.prepare_provider_accounting(&mut candidate, true);
        let prepared = h.prepare_ordinary_provider_terminal(&cid, &mut candidate, &mut projection);
        assert!(accounting.usage_prepared);
        assert!(!prepared.requested_tool_calls);
        assert_eq!(
            prepared.output_length.plan.is_some(),
            stop == tau_proto::ProviderStopReason::Length
        );
        assert_eq!(
            h.session_runtime.current_session_state.token_usage,
            before_stats
        );
        assert_eq!(event_log_events(&h).len(), before_events);
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            before_index
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .turn
                .terminal_status_was_available,
            before_status
        );
        assert_eq!(
            h.prompt_coordination.prompt_runtime.models[&source.agent_prompt_id],
            source.model
        );
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .estimated_cost_rates
                .contains_key(&source.agent_prompt_id)
        );
        h.handle_provider_response_finished(raw).expect("terminal");
        let canonical = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == source.agent_prompt_id =>
                {
                    Some(response)
                }
                _ => None,
            })
            .expect("canonical fact");
        assert_eq!(canonical, candidate);
        assert!(
            !h.prompt_coordination
                .prompt_runtime
                .models
                .contains_key(&source.agent_prompt_id)
        );
        h.shutdown().expect("shutdown");
    }
}

/// A missing continuation checkpoint still consumes its historical identity
/// before eager-compaction shaping; admission must see that exact later
/// identity, including saturation, without temporarily mutating the agent.
#[test]
fn complete_preparation_projects_continuation_cursor_into_automatic_decision() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let cid = h
        .agent_id_for_prompt(&source.agent_prompt_id)
        .expect("owner");
    let mut raw = reasoning_only_length_response(&source, 7);
    raw.agent_prompt_id = test_agent_prompt_id("missing-checkpoint");
    let usage = raw.usage.as_mut().expect("usage");
    usage.model = Some(source.model);
    usage.prompt_sent_tokens = 100;
    h.prompt_coordination
        .prompt_runtime
        .compaction_policies
        .insert(
            raw.agent_prompt_id.clone(),
            [(
                "threshold".to_owned(),
                tau_config::settings::CompactionPolicy {
                    enable: true,
                    threshold: CompactionPolicyThreshold::Tokens(50),
                    when: tau_config::settings::ContextPolicyWhen {
                        at: ContextPolicyPoint::OuterTurnFinished,
                        statuses: None,
                    },
                },
            )]
            .into(),
        );
    for index in [41_u64, u64::MAX] {
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .dispatch
            .next_prompt_index = index;
        let mut candidate = raw.clone();
        let mut projection = TerminalResponseProjection::from_response(&candidate);
        let prepared = h.prepare_ordinary_provider_terminal(&cid, &mut candidate, &mut projection);
        assert!(prepared.output_length.plan.is_none());
        assert_eq!(
            prepared.output_length.next_prompt_index,
            Some(index.saturating_add(1))
        );
        assert_eq!(
            candidate
                .automatic_compaction_decision
                .as_ref()
                .expect("decision")
                .transaction_id,
            tau_proto::CompactionTransactionId::parse(format!("ct-{}", index.saturating_add(1)))
                .expect("transaction")
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            index
        );
        h.apply_output_length_preparation(&cid, prepared.output_length);
        h.apply_automatic_compaction_preparation(&cid, prepared.automatic);
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            index.saturating_add(2)
        );
    }
    h.shutdown().expect("shutdown");
}

/// Preparing output-length authority must not spend its prompt identity or
/// install ownership; attachment must use exactly the prepared checkpoint.
#[test]
fn output_length_preparation_preserves_identity_and_matches_attachment() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let cid = h
        .agent_id_for_prompt(&source.agent_prompt_id)
        .expect("owner");
    let mut response = reasoning_only_length_response(&source, 7);
    let before = &h.agent_runtime.agent_registry.agents[&cid];
    let index = before.dispatch.next_prompt_index;
    let state = before.turn.output_length_continuation.clone();
    let events = event_log_events(&h).len();

    let prepared = h.prepare_output_length_continuation(
        &cid,
        &response,
        tau_proto::PromptOperation::Inference,
        false,
    );
    let repeated = h.prepare_output_length_continuation(
        &cid,
        &response,
        tau_proto::PromptOperation::Inference,
        false,
    );
    assert_eq!(prepared.disposition, repeated.disposition);
    assert_eq!(prepared.plan, repeated.plan);
    assert_eq!(prepared.next_prompt_index, Some(index + 1));
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_prompt_index,
        index
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .output_length_continuation,
        state
    );
    assert_eq!(event_log_events(&h).len(), events);
    assert_eq!(
        response.output_length_disposition,
        tau_proto::OutputLengthDisposition::None
    );
    let plan = prepared.plan.expect("checkpoint-backed plan");
    assert_eq!(plan.owner.source_agent_prompt_id, source.agent_prompt_id);
    assert_eq!(plan.dispatch.model, source.model);

    h.derive_output_length_continuation(
        &cid,
        &mut response,
        tau_proto::PromptOperation::Inference,
        false,
    );
    assert_eq!(response.output_length_disposition, prepared.disposition);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_prompt_index,
        index + 1
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .output_length_continuation,
        OutputLengthContinuationState::Planned(plan)
    );
    let spent = h.prepare_output_length_continuation(
        &cid,
        &response,
        tau_proto::PromptOperation::Inference,
        false,
    );
    assert_eq!(spent.disposition, tau_proto::OutputLengthDisposition::None);
    assert_eq!(spent.next_prompt_index, None);
    assert!(spent.plan.is_none());
    h.shutdown().expect("shutdown");
}

/// The existing derivation consumes an identity before discovering a missing
/// checkpoint. Preserve that unusual behavior, including saturated cursors,
/// while keeping preparation itself free of mutations.
#[test]
fn output_length_preparation_preserves_missing_checkpoint_identity_consumption() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let cid = h
        .agent_id_for_prompt(&source.agent_prompt_id)
        .expect("owner");
    let mut response = reasoning_only_length_response(&source, 7);
    response.agent_prompt_id = test_agent_prompt_id("unmarked-source");
    for index in [37, u64::MAX] {
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .dispatch
            .next_prompt_index = index;
        let prepared = h.prepare_output_length_continuation(
            &cid,
            &response,
            tau_proto::PromptOperation::Inference,
            false,
        );
        assert_eq!(
            prepared.disposition,
            tau_proto::OutputLengthDisposition::None
        );
        assert!(prepared.plan.is_none());
        assert_eq!(prepared.next_prompt_index, Some(index.saturating_add(1)));
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            index
        );
        h.derive_output_length_continuation(
            &cid,
            &mut response,
            tau_proto::PromptOperation::Inference,
            false,
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            index.saturating_add(1)
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .turn
                .output_length_continuation,
            OutputLengthContinuationState::None
        );
    }
    h.shutdown().expect("shutdown");
}

/// Eager decision preparation must preserve exact policy coalescing and
/// identity values, without allocating or publishing before its wrapper
/// applies.
#[test]
fn automatic_compaction_preparation_is_read_only_and_matches_attachment() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let cid = h
        .agent_id_for_prompt(&source.agent_prompt_id)
        .expect("owner");
    let policies = [("higher", 90), ("lower", 50), ("zero", 0)]
        .into_iter()
        .map(|(name, tokens)| {
            (
                name.to_owned(),
                tau_config::settings::CompactionPolicy {
                    enable: true,
                    threshold: CompactionPolicyThreshold::Tokens(tokens),
                    when: tau_config::settings::ContextPolicyWhen {
                        at: ContextPolicyPoint::OuterTurnFinished,
                        statuses: None,
                    },
                },
            )
        })
        .collect();
    let events = event_log_events(&h).len();
    for index in [23, u64::MAX] {
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .dispatch
            .next_prompt_index = index;
        let prepared = h.prepare_automatic_compaction_decision(
            &cid,
            source.model.clone(),
            Some(tau_proto::TokenCount::new(100)),
            Some(source.agent_prompt_id.clone()),
            &policies,
        );
        let repeated = h.prepare_automatic_compaction_decision(
            &cid,
            source.model.clone(),
            Some(tau_proto::TokenCount::new(100)),
            Some(source.agent_prompt_id.clone()),
            &policies,
        );
        assert_eq!(prepared.decision, repeated.decision);
        let diagnostic = prepared.diagnostic.expect("coalesced policies");
        assert_eq!(diagnostic.names, "higher,lower");
        assert_eq!(diagnostic.threshold, tau_proto::TokenCount::new(50));
        let decision = prepared.decision.expect("eligible decision");
        assert_eq!(
            decision.transaction_id,
            tau_proto::CompactionTransactionId::parse(format!("ct-{index}")).expect("transaction")
        );
        assert_eq!(decision.threshold, diagnostic.threshold);
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            index
        );
        assert_eq!(event_log_events(&h).len(), events);
        assert_eq!(
            h.eager_automatic_compaction_decision(
                &cid,
                source.model.clone(),
                Some(tau_proto::TokenCount::new(100)),
                Some(source.agent_prompt_id.clone()),
                &policies,
            ),
            Some(decision)
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            index.saturating_add(1)
        );
    }
    // Historical evidence has not committed. Matching policies still produce
    // the old diagnostic, but neither preparation nor attachment spends an id.
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .context_usage_prompt_id = Some(source.agent_prompt_id.clone());
    let rejected = h.prepare_automatic_compaction_decision(
        &cid,
        source.model.clone(),
        Some(tau_proto::TokenCount::new(100)),
        None,
        &policies,
    );
    assert!(rejected.decision.is_none());
    assert!(rejected.diagnostic.is_some());
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .next_prompt_index = 41;
    assert!(
        h.eager_automatic_compaction_decision(
            &cid,
            source.model.clone(),
            Some(tau_proto::TokenCount::new(100)),
            None,
            &policies,
        )
        .is_none()
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_prompt_index,
        41
    );
    let no_usage = h.prepare_automatic_compaction_decision(
        &cid,
        source.model,
        None,
        Some(source.agent_prompt_id),
        &policies,
    );
    assert!(no_usage.decision.is_none());
    assert!(no_usage.diagnostic.is_none());
    h.shutdown().expect("shutdown");
}

/// Candidate normalization must freeze repaired IDs and declarations without
/// registering any tool ownership; the existing wrapper installs exactly those
/// calls, preserving prior ownership and duplicate-call rejection.
#[test]
fn tool_field_preparation_is_read_only_and_matches_attachment() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let prior = test_agent_prompt_id("prior-owner");
    h.prompt_coordination
        .prompt_runtime
        .record_tool_call_prompt("reserved".into(), prior.clone());
    h.tool_routing
        .tool_runtime
        .completed_tool_calls
        .insert("reserved".into());
    let mut raw = reasoning_only_length_response(&source, 7);
    raw.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
    let raw_calls = ["valid", "valid", "reserved", ""]
        .map(|id| AgentToolCall {
            call_ref: None,
            id: id.into(),
            name: ToolName::new("missing_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })
        .to_vec();
    raw.output_items = raw_calls
        .iter()
        .map(|call| {
            ContextItem::ToolCall(ToolCallItem {
                call_id: call.id.clone(),
                name: call.name.clone(),
                tool_type: call.tool_type,
                arguments: call.arguments.clone(),
                raw_arguments_json: None,
                responses_envelope: None,
            })
        })
        .collect();
    let declaration = tau_proto::ObservationId::random();
    let events = event_log_events(&h).len();
    let mut candidate = raw.clone();
    let mut calls = raw_calls.clone();
    let prepared = h.prepare_finished_response_tool_calls(
        &mut candidate,
        &mut calls,
        false,
        false,
        declaration,
    );
    assert_eq!(prepared.invalid_errors.len(), 3);
    assert_eq!(prepared.calls.len(), 4);
    for (index, entry) in prepared.calls.iter().enumerate() {
        assert_eq!(
            entry.call.call_ref,
            Some(tau_proto::ToolCallRef {
                declaration,
                item_index: index as u32
            })
        );
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .tool_call_prompt(&entry.call.id)
                .is_none()
        );
    }
    assert_eq!(
        h.prompt_coordination
            .prompt_runtime
            .tool_call_prompt(&"reserved".into()),
        Some(&prior)
    );
    assert_eq!(event_log_events(&h).len(), events);
    let mut repeated_response = raw.clone();
    let mut repeated_calls = raw_calls.clone();
    let repeated = h.prepare_finished_response_tool_calls(
        &mut repeated_response,
        &mut repeated_calls,
        false,
        false,
        declaration,
    );
    assert_eq!(candidate, repeated_response);
    assert_eq!(prepared.invalid_errors, repeated.invalid_errors);

    let applied = h.normalize_finished_response_tool_calls(
        &mut raw,
        &mut raw_calls.clone(),
        false,
        false,
        declaration,
    );
    assert_eq!(raw, candidate);
    assert_eq!(applied.invalid_errors, prepared.invalid_errors);
    for (actual, expected) in applied.calls.iter().zip(&prepared.calls) {
        assert_eq!(actual.call.id, expected.call.id);
        assert_eq!(actual.call.call_ref, expected.call.call_ref);
        assert_eq!(actual.background_support, expected.background_support);
        assert_eq!(actual.turn_categories, expected.turn_categories);
        assert_eq!(
            h.prompt_coordination
                .prompt_runtime
                .tool_call_prompt(&actual.call.id),
            Some(&source.agent_prompt_id)
        );
    }
    assert_eq!(
        h.prompt_coordination
            .prompt_runtime
            .tool_call_prompt(&"reserved".into()),
        Some(&prior)
    );
    h.shutdown().expect("shutdown");
}

/// Background scheduling must use the aliased tool metadata frozen into the
/// originating prompt, not an unrelated internal-name collision or a later
/// replacement in the live registry.
#[test]
fn tool_background_policy_uses_prompt_owned_aliased_snapshot() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    let tool_connection = crate::test_connection_id("tools");
    let tool_sink =
        connect_ready_configured_extension(&mut h, "tools", "tools", tau_proto::ClientKind::Tool);
    let selected = background_policy_tool(
        "generic_asset",
        "render_asset",
        Some(tau_proto::BackgroundSupport::Never),
    );
    h.tool_routing
        .registry
        .register(&tool_connection, selected.clone());
    h.tool_routing.registry.register(
        &crate::test_connection_id("collision"),
        background_policy_tool(
            "render_asset",
            "other_asset",
            Some(tau_proto::BackgroundSupport::Instant),
        ),
    );
    h.submit_user_prompt(test_session_id("s1"), "render".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let frozen = h
        .resolve_enabled_tool_spec_for_prompt(
            &ToolName::new("render_asset"),
            &source.agent_prompt_id,
        )
        .expect("aliased tool is present in the prompt snapshot");
    assert_eq!(frozen.name.as_str(), "generic_asset");
    assert_eq!(
        frozen.background_support,
        Some(tau_proto::BackgroundSupport::Never)
    );

    h.tool_routing.registry.register(
        &tool_connection,
        background_policy_tool(
            "generic_asset",
            "render_asset",
            Some(tau_proto::BackgroundSupport::Instant),
        ),
    );
    let (mut response, mut calls) =
        response_with_one_tool_call(&source, "frozen-policy", "render_asset");
    let prepared = h.normalize_finished_response_tool_calls(
        &mut response,
        &mut calls,
        false,
        false,
        tau_proto::ObservationId::random(),
    );
    assert_eq!(prepared.calls.len(), 1);
    assert_eq!(
        prepared.calls[0].background_support,
        tau_proto::BackgroundSupport::Never
    );
    let cid = h
        .agent_id_for_prompt(&source.agent_prompt_id)
        .expect("prompt owner");
    h.dispatch_finished_response_tool_calls(&cid, prepared, None)
        .expect("dispatch aliased call");
    assert!(sink_has_tool_invoke(&tool_sink, "frozen-policy"));
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ToolStarted(started)
            if started.call_id.as_str() == "frozen-policy"
                && started.tool_name.as_str() == "generic_asset"
    )));
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .next_background_deadline()
            .is_none()
    );
    h.process_background_deadlines_at(Instant::now() + Duration::from_secs(3));
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "frozen-policy"
                && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
    )));
    h.handle_extension_event_inner(
        &tool_connection,
        test_tool_result("frozen-policy", "generic_asset"),
    )
    .expect("settle real aliased tool result");
    assert!(h.tool_routing.tool_runtime.tool_turn.is_empty());
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("frozen-policy")
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("frozen-policy")
    );

    let model = h.config.selected_model.as_ref().expect("selected model");
    let fresh_surface =
        h.gather_effective_tool_specs_for_role_model(h.config.selected_role.as_str(), Some(model));
    let fresh = fresh_surface
        .iter()
        .find(|spec| spec.name.as_str() == "generic_asset")
        .expect("replacement appears on a newly materialized surface");
    assert_eq!(
        fresh.background_support,
        Some(tau_proto::BackgroundSupport::Instant)
    );
    h.shutdown().expect("shutdown");
}

/// Prompt-selected metadata must preserve every policy variant exactly, while
/// an omitted policy or missing selected spec retains the two-second
/// non-authorizing scheduler default.
#[test]
fn tool_background_policy_preserves_exact_values_and_default() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "render".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let cases = [
        (
            Some(tau_proto::BackgroundSupport::Instant),
            tau_proto::BackgroundSupport::Instant,
        ),
        (
            Some(tau_proto::BackgroundSupport::MinForegroundSeconds(7)),
            tau_proto::BackgroundSupport::MinForegroundSeconds(7),
        ),
        (None, tau_proto::BackgroundSupport::MinForegroundSeconds(2)),
    ];
    for (index, (declared, expected)) in cases.into_iter().enumerate() {
        h.prompt_coordination.prompt_runtime.tool_specs.insert(
            source.agent_prompt_id.clone(),
            vec![background_policy_tool("internal", "visible", declared)],
        );
        let (mut response, mut calls) =
            response_with_one_tool_call(&source, &format!("policy-{index}"), "visible");
        let prepared = h.prepare_finished_response_tool_calls(
            &mut response,
            &mut calls,
            false,
            false,
            tau_proto::ObservationId::random(),
        );
        assert_eq!(prepared.calls[0].background_support, expected);
    }

    h.prompt_coordination
        .prompt_runtime
        .tool_specs
        .insert(source.agent_prompt_id.clone(), Vec::new());
    h.tool_routing.registry.register(
        &crate::test_connection_id("unauthorized"),
        background_policy_tool(
            "registered_only",
            "registered_only",
            Some(tau_proto::BackgroundSupport::Instant),
        ),
    );
    let (mut response, mut calls) =
        response_with_one_tool_call(&source, "missing-spec", "registered_only");
    let prepared = h.normalize_finished_response_tool_calls(
        &mut response,
        &mut calls,
        false,
        false,
        tau_proto::ObservationId::random(),
    );
    assert_eq!(
        prepared.calls[0].background_support,
        tau_proto::BackgroundSupport::MinForegroundSeconds(2)
    );
    assert!(
        h.resolve_enabled_tool_spec_for_prompt(
            &ToolName::new("registered_only"),
            &source.agent_prompt_id,
        )
        .is_none()
    );
    let cid = h
        .agent_id_for_prompt(&source.agent_prompt_id)
        .expect("prompt owner");
    h.dispatch_finished_response_tool_calls(&cid, prepared, None)
        .expect("existing unavailable-tool rejection path");
    assert!(h.tool_routing.tool_runtime.tool_turn.is_empty());
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ProviderToolError(error) if error.call_id.as_str() == "missing-spec"
    )));
    h.shutdown().expect("shutdown");
}
