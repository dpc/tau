//! Tests for compaction behavior.

use super::super::lifecycle::seed_restored_compaction_checkpoint;
use super::*;

fn append_byte_fit_text(h: &mut Harness, cid: &AgentId, text: String) {
    let agent_id = durable_agent_id_for_conversation(h, cid);
    h.publish_for_agent(
        cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: crate::parse_agent_id(&agent_id),
            text,
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
}

#[test]
fn standalone_prefix_byte_fit_matches_fully_materialized_context() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    append_byte_fit_text(&mut h, &cid, "safe".to_owned());
    let safe = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("safe head");
    append_byte_fit_text(&mut h, &cid, "x".repeat(4_096));
    let incident = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("incident head");
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.publish_for_agent(
        &cid,
        Event::AgentInitializationContextSet(tau_proto::AgentInitializationContextSet {
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: agent_id.clone(),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("init-byte-fit")
                .expect("initialization id"),
            agents_message: Some("a".repeat(1_024)),
            effective_skills: Vec::new(),
            agents_files: Vec::new(),
        }),
    );
    let tree = h
        .session_runtime
        .agent_store
        .agent(&agent_id)
        .expect("tree");
    let materialized_bytes = |head| {
        let mut context = crate::prompt::assemble_prompt_context_prefix_from(
            tree,
            Some(head),
            tau_proto::AgentHead::Node(head),
        )
        .expect("prefix")
        .context;
        context.blocks.insert(
            0,
            crate::prompt::initialization_agents_context_block(tree).expect("initialization"),
        );
        tau_proto::ByteCount::new(
            u64::try_from(serde_json::to_vec(&context).expect("serialize").len()).expect("length"),
        )
    };
    let safe_budget = materialized_bytes(safe);
    assert!(materialized_bytes(incident) > safe_budget);
    assert_eq!(
        h.fitting_automatic_compaction_cut(
            &agent_id,
            tau_proto::AgentHead::Node(incident),
            None,
            safe_budget,
        ),
        Some(tau_proto::AgentHead::Node(safe))
    );
    h.shutdown().expect("shutdown");
}

/// The xk8g incident's 164,126-token ordinary report must not cross a
/// 200,000-token policy merely because its normalized replay has more than 1.6
/// MiB of unmeasured structural suffix.
#[test]
fn provider_report_below_threshold_never_schedules_from_suffix_size() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        let cid = ensure_test_user_agent(&mut h);
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
        establish_exact_provider_usage(&mut h, &cid, 164_126);
        // Slightly exceed xk8g's exact 1,673,640-byte suffix so the fixture
        // also clears the binary 1.6 MiB boundary.
        const MINIMUM_REPLAY_BYTES: usize = 1_677_722;
        append_byte_fit_text(&mut h, &cid, "x".repeat(MINIMUM_REPLAY_BYTES));
        let agent_id = durable_agent_id_for_conversation(&h, &cid);
        let tree = h
            .session_runtime
            .agent_store
            .agent(&agent_id)
            .expect("agent tree");
        let suffix_heavy_context =
            crate::prompt::assemble_prompt_context_from(tree, tree.head()).context;
        assert!(
            serde_json::to_vec(&suffix_heavy_context)
                .expect("serialize normalized replay")
                .len()
                >= MINIMUM_REPLAY_BYTES,
            "the causal oracle must retain more than 1.6 MiB of exact byte evidence"
        );
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(200_000));
        assert!(!h.schedule_standalone_auto_compaction_for_activation(&cid, true, None));
        assert_eq!(
            event_log_count(&h, |event| matches!(
                event,
                Event::AgentStandaloneCompactionStarted(_)
            )),
            0
        );
        assert_eq!(
            event_log_count(&h, |event| matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
            )),
            0
        );
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    {
        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("cold resume");
        let cid = ensure_test_user_agent(&mut h);
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(200_000));
        info.standalone_compaction_prefix_budget = None;
        assert!(!h.schedule_standalone_auto_compaction_for_activation(&cid, true, None));
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("restart before inference");
    let cid = ensure_test_user_agent(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(200_000));
    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::user("xk8g restart inference".to_owned()),
    )
    .expect("dispatch inference");
    let latest = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentPromptCreated(prompt) => Some(prompt),
            _ => None,
        })
        .next_back()
        .expect("prompt");
    assert_eq!(latest.operation, tau_proto::PromptOperation::Inference);
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );
    h.shutdown().expect("shutdown");
}

/// Exact provider input at the token threshold must authorize one proactive
/// transaction even when the complete durable transcript is tiny in bytes.
#[test]
fn provider_report_at_threshold_schedules_once_despite_tiny_transcript() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    let cid = ensure_test_user_agent(&mut h);
    let evidence_prompt = establish_exact_provider_usage(&mut h, &cid, 200_000);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let tree = h
        .session_runtime
        .agent_store
        .agent(&agent_id)
        .expect("agent tree");
    let context = crate::prompt::assemble_prompt_context_from(tree, tree.head()).context;
    assert!(
        serde_json::to_vec(&context)
            .expect("serialize transcript")
            .len()
            < 16_384,
        "the reverse-asymmetry oracle requires bytes far below the token threshold"
    );
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(200_000));

    assert!(h.schedule_standalone_auto_compaction_for_activation(&cid, true, None));
    assert!(!h.schedule_standalone_auto_compaction_for_activation(&cid, true, None));
    let starts: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1);
    assert!(matches!(
        &starts[0].trigger,
        tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { evidence }
            if evidence.provider_prompt_id == evidence_prompt
                && evidence.provider_input_tokens == tau_proto::TokenCount::new(200_000)
                && evidence.threshold == tau_proto::TokenCount::new(200_000)
    ));
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::AgentPromptCreated(prompt)
                if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
        )),
        1
    );
    h.shutdown().expect("shutdown");
}

/// A user cancellation must not erase standalone-compaction continuation
/// ownership: that durable transaction requires its own terminal recovery path.
#[test]
fn cancel_while_thinking_keeps_standalone_dispatch_ownership() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-cancel-standalone-thinking");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-cancel-standalone").expect("valid id");
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
    agent.dispatch.in_flight_prompt = Some(spid.clone());
    agent.dispatch.activation_dispatch =
        path_crate_agent::ActivationDispatchState::DispatchUncertain {
            owner: path_crate_agent::InferenceCheckpointOwner::Standalone {
                id: transaction_id.clone(),
            },
            agent_prompt_id: spid.clone(),
            through: tau_proto::AgentHead::Root,
            model: Some("test/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
        };
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());

    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
            agent_prompt_id: Some(spid.clone()),
        },
    );

    assert!(matches!(
        &h.agent_runtime.agent_registry.agents[&cid].dispatch.activation_dispatch,
        crate::agent::ActivationDispatchState::DispatchUncertain {
            owner: crate::agent::InferenceCheckpointOwner::Standalone { id },
            agent_prompt_id,
            ..
        } if *id == transaction_id && *agent_prompt_id == spid
    ));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptTerminated(terminated)
            if terminated.agent_prompt_id == spid
                && terminated.reason == tau_proto::AgentPromptTerminationReason::Canceled
    )));

    h.shutdown().expect("shutdown");
}

/// The proactive scheduler and context-limit telemetry must both reject a
/// same-model usage baseline whose producing node belongs to a sibling branch.
#[test]
fn off_branch_usage_baseline_is_ineligible_for_scheduling_and_telemetry() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(10_000));
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    for text in ["branch A", "branch B"] {
        h.publish_for_agent(
            &cid,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: agent_id.clone(),
                text: text.to_owned(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        );
        if text == "branch A" {
            let branch_a = h.agent_runtime.agent_registry.agents[&cid]
                .identity
                .head
                .expect("branch A");
            h.publish_for_agent(
                &cid,
                Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                    agent_id: agent_id.clone(),
                    head: tau_proto::AgentHead::Root,
                }),
            );
            h.agent_runtime
                .agent_registry
                .agents
                .get_mut(&cid)
                .expect("agent")
                .execution
                .context_usage_head = Some(branch_a);
        }
    }
    {
        let agent = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        agent.execution.context_input_tokens = Some(10_000);
        agent.execution.context_cached_tokens = Some(5_000);
        agent.execution.context_usage_model = Some("test/model".into());
        agent.execution.context_usage_prompt_id =
            Some(test_agent_prompt_id("ap-test-provider-usage"));
    }

    assert!(!h.schedule_standalone_auto_compaction_for_activation(&cid, true, None));
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .context_input_tokens = None;
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );
    h.shutdown().expect("shutdown");
}

/// A private local-compaction envelope on ordinary inference is rejected before
/// accounting or durable response mutation, and even deferred terminalization
/// retains no private body.
#[test]
fn ordinary_inference_rejects_private_compaction_output_before_persistence() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("ordinary prompt".to_owned()))
        .expect("dispatch ordinary prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    let usage_before = h.session_runtime.current_session_state.token_usage.clone();
    let durable_responses_before = loaded_agent_events(&h, "s1")
        .iter()
        .filter(|event| matches!(event, Event::ProviderResponseFinished(_)))
        .count();

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: prompt.agent_prompt_id.clone(),
        agent_id: agent_id.clone(),
        output_items: vec![ContextItem::LocalCompactionNarrative(
            tau_proto::LocalCompactionNarrativeItem {
                narrative: "must not persist".to_owned(),
            },
        )],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: Some(tau_proto::ProviderTokenUsage {
            model: None,
            prompt_sent_tokens: 9,
            prompt_cached_tokens: 3,
            prompt_cache_read_ceiling_tokens: None,
            cache: None,
            response_received_tokens: 2,
            stats: Default::default(),
        }),
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("private ordinary output is rejected in band");

    assert_eq!(
        h.session_runtime.current_session_state.token_usage,
        usage_before
    );
    assert_eq!(
        loaded_agent_events(&h, "s1")
            .iter()
            .filter(|event| matches!(event, Event::ProviderResponseFinished(_)))
            .count(),
        durable_responses_before
    );
    assert!(
        h.session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("tree")
            .nodes()
            .iter()
            .all(|node| !matches!(
                &node.entry,
                tau_core::AgentEntry::AssistantResponse { output_items, .. }
                    if output_items.iter().any(|item| matches!(
                        item,
                        ContextItem::LocalCompactionNarrative(_)
                    ))
            ))
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_stale_provider_responses
            .get(&prompt.agent_prompt_id)
            .is_none_or(|pending| pending.response.output_items.is_empty())
    );
    assert!(event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::AgentPromptTerminated(terminated)
                if terminated.agent_prompt_id == prompt.agent_prompt_id
        )
    }));
}

/// Canonical usage preserves only cache-read ceilings consistent with raw
/// counters.
#[test]
fn cache_read_ceiling_validation_preserves_valid_and_rejects_invalid_values() {
    assert_eq!(
        super::super::super::validate_cache_read_ceiling(4_096, 3_584, Some(3_584)),
        Some(3_584)
    );
    assert_eq!(
        super::super::super::validate_cache_read_ceiling(4_096, 3_584, Some(3_000)),
        None
    );
    assert_eq!(
        super::super::super::validate_cache_read_ceiling(4_096, 3_584, Some(5_000)),
        None
    );
    assert_eq!(
        super::super::super::validate_cache_read_ceiling(4_096, 3_584, None),
        None
    );
}
/// A tool-result activation checkpoint must capture the normalized prefix so a
/// reactive overflow cannot resend a compact request ending in a dangling call.
#[test]
fn reactive_context_overflow_after_tool_round_uses_closed_prefix() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: crate::parse_agent_id(&agent_id),
            text: "reactive prefix".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let prefix = tau_proto::AgentHead::Node(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("prefix"),
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: test_agent_prompt_id("ap-reactive-tool"),
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-reactive".into(),
                name: ToolName::new("reactive_tool"),
                tool_type: tau_proto::ToolType::Custom,
                arguments: CborValue::Text("input".to_owned()),
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
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-reactive".into(),
            tool_name: ToolName::new("reactive_tool"),
            tool_type: tau_proto::ToolType::Custom,
            result: CborValue::Text("reactive output".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    );
    let results = tau_proto::AgentHead::Node(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("results"),
    );
    h.dispatch_activation_after_publish_idle(&cid);
    let inference = read_nth_prompt_created(&h, 0);
    let checkpoint = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == inference.agent_prompt_id =>
            {
                Some(started)
            }
            _ => None,
        })
        .expect("inference checkpoint");
    assert_eq!(checkpoint.activation_cut, Some(prefix));
    assert_eq!(checkpoint.through, results);

    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("start reactive compaction");
    let compact = read_nth_prompt_created(&h, 1);
    assert!(
        compact
            .context
            .flatten_iter()
            .all(|item| !matches!(item, ContextItem::ToolCall(_) | ContextItem::ToolResult(_)))
    );
    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .expect("reactive compaction start");
    assert_eq!(started.cut, prefix);
    assert_eq!(started.resume_through, Some(results));

    h.handle_provider_response_finished(provider_text_response(
        &compact.agent_prompt_id,
        compact.agent_id,
        "reactive replacement",
    ))
    .expect("accept reactive compaction");
    let continuation = read_nth_prompt_created(&h, 2);
    let timeline: Vec<_> = continuation.context.flatten_iter().collect();
    assert_eq!(
        timeline
            .iter()
            .filter(|item| matches!(item, ContextItem::ToolCall(_)))
            .count(),
        1
    );
    assert_eq!(
        timeline
            .iter()
            .filter(|item| matches!(item, ContextItem::ToolResult(_)))
            .count(),
        1
    );
    h.shutdown().expect("shutdown");
}
/// A readiness-deferred activation stays dormant after root navigation instead
/// of publishing an invalid off-branch inference checkpoint, then dispatches
/// when its owning branch is reselected.
#[test]
fn readiness_deferred_activation_is_branch_owned_below_compaction_threshold() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = false;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(10_000));
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .context_input_tokens = Some(0);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .context_usage_model = Some("test/model".into());
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .context_usage_prompt_id = Some(test_agent_prompt_id("ap-test-provider-usage"));
    set_test_agent_context_wait(
        &mut h,
        agent_id.clone(),
        path_std_collections::HashSet::from([tau_proto::ConnectionId::parse("context-provider")
            .expect("test connection id must satisfy the identifier grammar")]),
    );

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("branch A activation".to_owned()))
        .expect("park branch A activation");
    let branch_a = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("branch A activation node");
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    finish_test_agent_context_wait(&mut h, &agent_id);
    h.drain_publish_idle_dispatches();

    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentInferenceDispatchStarted(_)))
    );
    assert!(
        matches!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .activation_dispatch,
            crate::agent::ActivationDispatchState::None
        ),
        "off-branch state: {:?}",
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch
    );
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);

    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: tau_proto::AgentHead::Node(branch_a),
        }),
    );
    let checkpoint = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentInferenceDispatchStarted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .expect("reselected activation checkpoint");
    assert_eq!(checkpoint.through, tau_proto::AgentHead::Node(branch_a));
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
}
/// A done-policy decision on a post-tool continuation remains owned by the
/// original outer turn and starts exactly one standalone transaction.
#[test]
fn outer_turn_finished_done_policy_persists_and_starts_one_compaction() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    for name in ["eager-high", "eager-low"] {
        role.compactions.insert(
            name.to_owned(),
            tau_config::settings::CompactionPolicy {
                threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(
                    if name == "eager-low" { 100 } else { 200 },
                ),
                enable: true,
                when: tau_config::settings::ContextPolicyWhen {
                    at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
                    statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
                },
            },
        );
    }
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("finish".to_owned()))
        .expect("dispatch inference");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(provider_tool_response(
        &inference,
        "finish-round-tool",
        "self_info",
        CborValue::Map(Vec::new()),
    ))
    .expect("finish tool round");
    let continuation = read_nth_prompt_created(&h, 1);
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(tau_proto::AgentWorkStatusPhase::Done, "finished".to_owned())
            .expect("valid status"),
    )
    .expect("report status");
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role")
        .compactions
        .clear();
    let mut response =
        provider_text_response(&continuation.agent_prompt_id, continuation.agent_id, "done");
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 250,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 1,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(response)
        .expect("finish inference");

    let events = event_log_events(&h);
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty(),
        "accepted continuation must not enter the retained publication retry path"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == continuation.agent_prompt_id
            ))
            .count(),
        1,
        "the canonical continuation terminal must commit exactly once"
    );
    let decision = events
        .iter()
        .find_map(|event| match event {
            Event::ProviderResponseFinished(response) => {
                response.automatic_compaction_decision.clone()
            }
            _ => None,
        })
        .expect("terminal decision");
    let outer_turn_id = events
        .iter()
        .find_map(|event| match event {
            Event::AgentOuterTurnStarted(started)
                if started.agent_prompt_id == inference.agent_prompt_id =>
            {
                Some(started.outer_turn_id.clone())
            }
            _ => None,
        })
        .expect("outer turn start");
    assert_ne!(continuation.agent_prompt_id, inference.agent_prompt_id);
    assert_eq!(decision.outer_turn_id, outer_turn_id);
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentPromptStarted(started)
            if started.agent_prompt_id == continuation.agent_prompt_id
                && started.outer_turn_id.as_ref() == Some(&outer_turn_id)
    )));
    assert_eq!(
        decision.threshold,
        tau_proto::TokenCount::new(100),
        "matching policies coalesce to minimum"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentOuterTurnFinished(finish)
            if finish.automatic_compaction_decision.as_ref()
                == Some(&decision.transaction_id)
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        &started.trigger,
                        tau_proto::StandaloneCompactionTrigger::AutomaticPolicy {
                            decision_id
                        } if decision_id == &decision.transaction_id
                    )
            ))
            .count(),
        1
    );
    h.shutdown().expect("shutdown");
}

/// The legacy CLI threshold remains a compound edit: it updates inline/reactive
/// policy and the named default without erasing siblings or default selectors.
#[test]
fn legacy_role_threshold_update_preserves_named_compaction_siblings() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let role_name = h.config.selected_role.clone();
    let role = h
        .config
        .available_roles
        .get_mut(&role_name)
        .expect("selected role");
    role.compactions.insert(
        "default".to_owned(),
        tau_config::settings::CompactionPolicy {
            threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(90_000),
            enable: false,
            when: tau_config::settings::ContextPolicyWhen {
                at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
                statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
            },
        },
    );
    role.compactions.insert(
        "eager".to_owned(),
        tau_config::settings::CompactionPolicy {
            threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(160_000),
            enable: true,
            when: tau_config::settings::ContextPolicyWhen {
                at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
                statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
            },
        },
    );
    h.handle_ui_role_update(
        crate::harness::harness_connection_id(),
        tau_proto::UiRoleUpdate {
            role: role_name.clone(),
            action: tau_proto::UiRoleUpdateAction::SetCompactionThreshold {
                compaction_threshold: Some(tau_proto::TokenCount::new(120_000)),
            },
        },
    )
    .expect("update threshold");

    let role = &h.config.available_roles[&role_name];
    assert_eq!(
        role.inference_compaction,
        Some(tau_config::settings::RoleCompaction::Threshold(120_000))
    );
    assert_eq!(
        role.compactions["default"].threshold,
        path_tau_config_settings::CompactionPolicyThreshold::Tokens(120_000)
    );
    assert_eq!(
        role.compactions["default"].when,
        tau_config::settings::ContextPolicyWhen {
            at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
            statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
        }
    );
    assert!(role.compactions["default"].enable);
    assert!(role.compactions.contains_key("eager"));
    h.shutdown().expect("shutdown");
}

/// Explicit `:compact` must repair a warm historical open-prefix failure,
/// retain queued activation ownership, and dispatch the exact tool round once.
#[test]
fn compact_repairs_warm_historical_open_prefix_failure() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");
    let (prefix, assistant, results) =
        seed_historical_open_prefix_failure(&mut h, &cid, "test/model");
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(crate::parse_agent_id(&agent_id)),
            agent_prompt_id: None,
        },
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert!(!event_log_events(&h).into_iter().any(|event| matches!(
        event,
        Event::HarnessNotice(notice) if notice.message == "no active turn to cancel"
    )));

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("queued while blocked".to_owned()))
        .expect("queue activating input");
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        1,
        "new input must not implicitly retry or clear suppression"
    );
    let queued = read_nth_prompt_created(&h, 1);
    h.handle_provider_response_finished(provider_text_response(
        &queued.agent_prompt_id,
        queued.agent_id,
        "queued input remains usable",
    ))
    .expect("finish queued input before explicit compaction");
    h.drain_publish_idle_dispatches();
    h.try_advance_queue();
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let starts: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0].cut, assistant);
    assert_eq!(starts[0].resume_through, Some(results));
    assert_eq!(starts[1].cut, prefix);
    assert_eq!(
        starts[1].supersedes.as_ref(),
        Some(&starts[0].transaction_id)
    );
    assert!(starts[1].resume_through.is_some());

    let retry = read_nth_prompt_created(&h, 2);
    assert!(
        retry
            .context
            .flatten_iter()
            .all(|item| !matches!(item, ContextItem::ToolCall(_) | ContextItem::ToolResult(_)))
    );
    h.handle_provider_response_finished(provider_text_response(
        &retry.agent_prompt_id,
        retry.agent_id,
        "recovered summary",
    ))
    .expect("accept corrected retry");
    let inference = read_nth_prompt_created(&h, 3);
    let rendered = serde_json::to_string(&inference.context).expect("context");
    assert_eq!(rendered.matches("call-historical").count(), 2);
    assert_eq!(rendered.matches("queued while blocked").count(), 1);
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::Inference
            ))
            .count(),
        2
    );
    h.shutdown().expect("shutdown");
}

/// Cold replay must retain the failed transaction and allow `:compact` to
/// supersede its historical open cut with the normalized ancestor.
#[test]
fn compact_repairs_cold_historical_open_prefix_failure() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (agent_id, prefix, failed_transaction);
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("model")
            .supports_standalone_compaction = true;
        let cid = ensure_test_user_agent(&mut h);
        agent_id = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .clone()
            .expect("durable agent");
        (prefix, _, _) = seed_historical_open_prefix_failure(&mut h, &cid, "test/model");
        failed_transaction = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::AgentStandaloneCompactionStarted(started) => Some(started.transaction_id),
                _ => None,
            })
            .expect("historical start");
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    enable_remote_compaction_for_test_model(&mut resumed);
    resumed
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .supports_standalone_compaction = true;
    resumed.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let successor = event_log_events(&resumed)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .next_back()
        .expect("successor start");
    assert_eq!(successor.cut, prefix);
    assert_eq!(successor.supersedes, Some(failed_transaction));
    assert!(successor.resume_through.is_some());
    let retry = read_nth_prompt_created(&resumed, 0);
    resumed
        .handle_provider_response_finished(provider_text_response(
            &retry.agent_prompt_id,
            retry.agent_id,
            "cold recovered summary",
        ))
        .expect("accept cold recovery");
    let continuation = read_nth_prompt_created(&resumed, 1);
    assert_eq!(
        continuation.operation,
        tau_proto::PromptOperation::Inference
    );
    assert!(!matches!(
        resumed
            .agent_runtime
            .agent_registry
            .agents
            .values()
            .find(|agent| agent.identity.agent_id.as_deref() == Some(agent_id.as_str()))
            .expect("recovered agent")
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    resumed.shutdown().expect("shutdown");
}

/// Navigation changes failure authority, so `:compact` starts a fresh warm
/// transaction instead of superseding the failure from the old branch.
#[test]
fn compact_starts_fresh_warm_transaction_after_branch_change() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");
    let (prefix, _, _) = seed_historical_open_prefix_failure(&mut h, &cid, "test/model");
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: crate::parse_agent_id(&agent_id),
            head: prefix,
        }),
    );

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        2
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::Running { .. }
    ));
    h.shutdown().expect("shutdown");
}

/// Cold replay preserves branch-qualified suppression and permits a fresh
/// explicit transaction after navigation changes authority.
#[test]
fn compact_starts_fresh_cold_transaction_after_branch_change() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let agent_id;
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("model")
            .supports_standalone_compaction = true;
        let cid = ensure_test_user_agent(&mut h);
        agent_id = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .clone()
            .expect("durable agent");
        let (prefix, _, _) = seed_historical_open_prefix_failure(&mut h, &cid, "test/model");
        h.publish_for_agent(
            &cid,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: crate::parse_agent_id(&agent_id),
                head: prefix,
            }),
        );
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    enable_remote_compaction_for_test_model(&mut resumed);
    resumed
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .supports_standalone_compaction = true;
    resumed.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    assert_eq!(
        event_log_events(&resumed)
            .into_iter()
            .filter(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        1,
        "changed branch authority must emit one fresh start"
    );
    let recovered = resumed
        .agent_runtime
        .agent_registry
        .agents
        .values()
        .find(|agent| agent.identity.agent_id.as_deref() == Some(agent_id.as_str()))
        .expect("replayed agent");
    assert!(matches!(
        recovered.dispatch.activation_dispatch,
        crate::agent::ActivationDispatchState::Running { .. }
    ));
    resumed.shutdown().expect("shutdown");
}

/// A self-compaction request must remain accepted-but-unstarted while any
/// foreground sibling is unresolved, then start from the one complete
/// ToolResults node after the final sibling folds.
#[test]
fn manual_self_compaction_waits_for_complete_sibling_round() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    seed_assistant_tool_round(
        &mut h,
        &cid,
        &[("call-compact", "compact"), ("call-sibling", "sibling")],
    );
    for (call_id, name) in [("call-compact", "compact"), ("call-sibling", "sibling")] {
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert(call_id.into(), cid.clone());
        h.tool_routing.tool_runtime.pending_tools.insert(
            call_id.into(),
            PendingTool {
                name: ToolName::new(name),
                internal_name: ToolName::new(name),
                tool_type: tau_proto::ToolType::Function,
                allows_provider_image: false,
            },
        );
        h.tool_routing
            .tool_runtime
            .tool_turn
            .record_unqueued_in_flight(cid.clone(), call_id.into(), ToolTurnCategories::default());
        h.prompt_coordination
            .prompt_runtime
            .tool_call_prompts
            .insert(call_id.into(), test_agent_prompt_id("sp-seeded-tools"));
    }
    let compact_call = AgentToolCall {
        call_ref: None,
        id: "call-compact".into(),
        name: ToolName::new("compact"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    };
    h.request_agent_tool_compaction(&cid, &compact_call, ToolName::new("compact"), None);

    assert!(
        event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::AgentManualCompactionRequested(_)
        )),
        "tracked={} backgrounded={} events={:?}",
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&compact_call.id),
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&compact_call.id),
        event_log_events(&h)
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentStandaloneCompactionStarted(_)
    )));

    h.finish_prebuilt_internal_tool_result(ToolResult {
        presentation: Default::default(),
        call_id: "call-sibling".into(),
        tool_name: ToolName::new("sibling"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("done".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });

    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .expect("compaction starts after sibling terminal");
    let tree = h
        .session_runtime
        .agent_store
        .agent(started.agent_id.as_str())
        .expect("agent tree");
    assert!(tree.has_complete_tool_round_for(started.cut.as_option(), &compact_call.id));
    let suffix_end = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .and_then(|agent| agent.identity.head)
        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
    h.publish_for_agent(
        &cid,
        Event::AgentCompacted(tau_proto::AgentCompacted {
            original_input_tokens: None,
            compaction_output_tokens: None,
            agent_id: started.agent_id.clone(),
            transaction_id: Some(started.transaction_id.clone()),
            cut: Some(started.cut),
            suffix_end: Some(suffix_end),
            compact_prompt_id: Some(started.compact_prompt_id.clone()),
            model: Some(started.model.clone()),
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
    );
    let events = event_log_events(&h);
    let background_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::ToolBackgroundResult(result) if result.call_id == compact_call.id
            )
        })
        .expect("original compact call completes in background");
    let checkpoint_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentInferenceDispatchStarted(checkpoint)
                    if checkpoint.transaction_id.as_ref() == Some(&started.transaction_id)
            )
        })
        .expect("self compaction checkpoints continuation");
    assert!(background_index < checkpoint_index);
    h.shutdown().expect("shutdown");
}

/// A real scheduler-owned self-compaction must advance through its ordinary
/// continuation, survive joined shutdown and cold replay without changing
/// prompt counters or duplicating correlated facts, and admit a second real
/// `compact` call only after a newer ordinary provider generation exists.
#[test]
fn scheduler_self_compaction_remains_eligible_after_cold_ordinary_turn() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let first_call_id = ToolCallId::from("scheduler-compact-first");
    let second_call_id = ToolCallId::from("scheduler-compact-second");

    let mut h = echo_harness(&state).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    install_scheduler_compaction_tools(&mut h);
    h.submit_user_prompt(test_session_id("s1"), "first ordinary turn".to_owned())
        .expect("submit first ordinary turn");
    let caller_cid = test_user_agent(&h);
    let caller_id = durable_agent_id_for_conversation(&h, &caller_cid);
    let first_inference = read_nth_prompt_created(&h, 0);

    h.handle_provider_response_finished(provider_compact_call(&first_inference, &first_call_id))
        .expect("accept first compact call");

    assert_eq!(
        h.tool_routing.tool_runtime.tool_agents.get(&first_call_id),
        Some(&caller_cid),
        "scheduler must commit the real call's caller ownership"
    );
    let first_records = h
        .session_runtime
        .agent_store
        .agent_events(caller_id.as_str())
        .expect("caller records");
    let first_response_index = first_records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == first_inference.agent_prompt_id
                        && response.output_items.iter().any(|item| matches!(
                            item,
                            ContextItem::ToolCall(call) if call.call_id == first_call_id
                        ))
            )
        })
        .expect("durable provider tool call");
    let first_placeholder_indices = first_records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(
                &record.event,
                Event::ProviderToolResult(result)
                    if result.call_id == first_call_id
                        && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(first_placeholder_indices.len(), 1);
    assert!(first_response_index < first_placeholder_indices[0]);

    let first_requests = first_records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentManualCompactionRequested(request)
                if request.required_tool_source().initiating_tool_call_id == first_call_id =>
            {
                Some(request.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(first_requests.len(), 1);
    let first_request = &first_requests[0];
    assert_eq!(
        first_request.required_tool_source().caller_agent_id,
        caller_id
    );
    assert_eq!(first_request.target_agent_id, caller_id);
    assert_eq!(
        first_request
            .required_tool_source()
            .initiating_agent_prompt_id,
        first_inference.agent_prompt_id
    );
    let first_starts = first_records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentStandaloneCompactionStarted(started)
                if matches!(
                    &started.trigger,
                    tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                        request_id,
                        caller_agent_id,
                        initiating_tool_call_id,
                    } if request_id == &first_request.request_id
                        && caller_agent_id == &caller_id
                        && initiating_tool_call_id == &first_call_id
                ) =>
            {
                Some(started.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(first_starts.len(), 1);
    let first_start = &first_starts[0];
    let first_compaction = h
        .read_agent_prompt_created(
            &h.session_runtime.current_session_id,
            &first_start.compact_prompt_id,
        )
        .expect("first standalone prompt");
    assert_eq!(
        first_compaction.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );

    h.handle_provider_response_finished(provider_text_response(
        &first_compaction.agent_prompt_id,
        first_compaction.agent_id,
        "first compacted summary",
    ))
    .expect("finish first standalone compaction");

    let first_continuation = read_nth_prompt_created(&h, 2);
    assert_eq!(
        first_continuation.operation,
        tau_proto::PromptOperation::Inference
    );
    let first_records = h
        .session_runtime
        .agent_store
        .agent_events(caller_id.as_str())
        .expect("caller records after compaction");
    let prompt_operations = first_records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentPromptStarted(prompt) => Some(prompt.operation),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        prompt_operations,
        vec![
            tau_proto::PromptOperation::Inference,
            tau_proto::PromptOperation::StandaloneCompaction,
            tau_proto::PromptOperation::Inference,
        ]
    );
    let first_tree = h
        .session_runtime
        .agent_store
        .agent(caller_id.as_str())
        .expect("caller tree");
    assert_eq!(
        first_tree.ordinary_inference_generation(),
        tau_proto::MaterializedPromptGeneration::from_inference_generation(2)
    );

    let first_background_index = first_records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::ToolBackgroundResult(result) if result.call_id == first_call_id
            )
        })
        .expect("first background result");
    let first_checkpoint_index = first_records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::AgentInferenceDispatchStarted(checkpoint)
                    if checkpoint.transaction_id.as_ref()
                        == Some(&first_start.transaction_id)
            )
        })
        .expect("first continuation checkpoint");
    let first_checkpoint = match &first_records[first_checkpoint_index].event {
        Event::AgentInferenceDispatchStarted(checkpoint) => checkpoint,
        _ => unreachable!("matched checkpoint"),
    };
    let first_continuation_index = first_records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::AgentPromptStarted(prompt)
                    if prompt.agent_prompt_id == first_checkpoint.agent_prompt_id
                        && prompt.operation == tau_proto::PromptOperation::Inference
            )
        })
        .expect("first continuation dispatch");
    assert!(first_background_index < first_checkpoint_index);
    assert!(first_checkpoint_index < first_continuation_index);
    assert_eq!(
        first_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ToolBackgroundResult(result) if result.call_id == first_call_id
            ))
            .count(),
        1
    );

    h.handle_provider_response_finished(provider_text_response(
        &first_continuation.agent_prompt_id,
        first_continuation.agent_id,
        "ordinary continuation complete",
    ))
    .expect("finish ordinary continuation");
    let first_request_id = first_request.request_id.clone();
    let first_transaction_id = first_start.transaction_id.clone();
    h.shutdown().expect("joined shutdown");
    drop(h);
    assert!(
        !tau_core::session_is_locked(&tau_config::settings::sessions_dir_of(&state), "s1")
            .expect("session lock probe"),
        "joined shutdown and drop must release the session lock"
    );

    let mut resumed =
        echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
            .expect("cold reopen");
    resumed
        .provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    install_scheduler_compaction_tools(&mut resumed);
    let resumed_cid = resumed
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(caller_id.as_str())
        .cloned()
        .expect("restored caller");
    let reopened_records = resumed
        .session_runtime
        .agent_store
        .agent_events(caller_id.as_str())
        .expect("reopened caller records");
    let durable_materialized = reopened_records
        .iter()
        .filter(|record| matches!(record.event, Event::AgentPromptStarted(_)))
        .count() as u64;
    let durable_ordinary = reopened_records
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentPromptStarted(prompt)
                    if prompt.operation == tau_proto::PromptOperation::Inference
            )
        })
        .count() as u64;
    let reopened_tree = resumed
        .session_runtime
        .agent_store
        .agent(caller_id.as_str())
        .expect("reopened caller tree");
    assert_eq!(
        reopened_tree.ordinary_inference_generation(),
        tau_proto::MaterializedPromptGeneration::from_inference_generation(durable_ordinary)
    );
    assert_eq!((durable_materialized, durable_ordinary), (3, 2));

    resumed
        .submit_user_prompt(
            test_session_id("s1"),
            "new ordinary turn after cold reopen".to_owned(),
        )
        .expect("submit post-reopen ordinary turn");
    let second_inference = read_nth_prompt_created(&resumed, 0);
    assert_eq!(
        second_inference.operation,
        tau_proto::PromptOperation::Inference
    );
    resumed
        .handle_provider_response_finished(provider_compact_call(
            &second_inference,
            &second_call_id,
        ))
        .expect("accept second compact call");

    let final_records = resumed
        .session_runtime
        .agent_store
        .agent_events(caller_id.as_str())
        .expect("final caller records");
    let second_requests = final_records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentManualCompactionRequested(request)
                if request.required_tool_source().initiating_tool_call_id == second_call_id =>
            {
                Some(request)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(second_requests.len(), 1);
    let second_request = second_requests[0];
    assert_eq!(
        second_request.required_tool_source().caller_agent_id,
        caller_id
    );
    assert_eq!(second_request.target_agent_id, caller_id);
    assert_eq!(
        second_request
            .required_tool_source()
            .initiating_agent_prompt_id,
        second_inference.agent_prompt_id
    );
    assert!(
        second_request.target_generation
            > tau_proto::MaterializedPromptGeneration::from_inference_generation(durable_ordinary)
    );
    assert_ne!(second_request.request_id, first_request_id);
    let second_starts = final_records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentStandaloneCompactionStarted(started)
                if matches!(
                    &started.trigger,
                    tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                        request_id,
                        initiating_tool_call_id,
                        ..
                    } if request_id == &second_request.request_id
                        && initiating_tool_call_id == &second_call_id
                ) =>
            {
                Some(started)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(second_starts.len(), 1);
    assert_ne!(second_starts[0].transaction_id, first_transaction_id);
    assert_eq!(
        final_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ProviderToolResult(result)
                    if result.call_id == second_call_id
                        && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
            ))
            .count(),
        1
    );
    assert!(!event_log_events(&resumed).iter().any(|event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id == second_call_id && error.message == "not_needed"
    )));

    assert_eq!(
        final_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentManualCompactionRequested(request)
                    if request.request_id == first_request_id
            ))
            .count(),
        1
    );
    assert_eq!(
        final_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentStandaloneCompactionStarted(started)
                    if started.transaction_id == first_transaction_id
            ))
            .count(),
        1
    );
    assert_eq!(
        final_records
            .iter()
            .filter(|record| match &record.event {
                Event::AgentCompacted(compacted) => {
                    compacted.transaction_id.as_ref() == Some(&first_transaction_id)
                }
                Event::AgentStandaloneCompactionFailed(failed) => {
                    failed.transaction_id == first_transaction_id
                }
                _ => false,
            })
            .count(),
        1
    );
    assert_eq!(
        final_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ToolBackgroundResult(result) if result.call_id == first_call_id
            ))
            .count(),
        1
    );
    assert_eq!(
        resumed
            .tool_routing
            .tool_runtime
            .tool_agents
            .get(&second_call_id),
        Some(&resumed_cid)
    );
    resumed.shutdown().expect("shutdown");
}

/// The real scheduler path for `compact` must consume the built-in tool's
/// custom placeholder reservation instead of publishing a second placeholder,
/// keep foreground and wait state pending while interception parks that
/// placeholder, and keep ordinary publication live after commit.
#[test]
fn scheduler_compact_publishes_one_placeholder_and_keeps_publication_live() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    install_scheduler_compaction_tools(&mut h);
    let _interceptor = connect_test_tool(&mut h, "compact-placeholder-interceptor");
    h.handle_extension_event(
        "compact-placeholder-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_TOOL_RESULT,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register placeholder interceptor");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    let prompt_id: AgentPromptId = test_agent_prompt_id("sp-scheduler-compact");
    seed_agent_thinking(&mut h, &cid, prompt_id.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(prompt_id.clone(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: prompt_id,
        agent_id: crate::parse_agent_id(&agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "scheduler-compact".into(),
            name: ToolName::new("compact"),
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
    .expect("scheduler dispatch");

    assert!(matches!(
        h.runtime_io.publication.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::ProviderToolResult(result))
            if result.call_id.as_str() == "scheduler-compact"
                && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
    ));
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&ToolCallId::from("scheduler-compact")),
        "parking the placeholder must also park dependent state"
    );
    assert!(!h.wait_call_is_backgrounded_for_test(&ToolCallId::from("scheduler-compact")));
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == "scheduler-compact"
        )),
        0
    );
    h.handle_extension_event(
        "compact-placeholder-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit placeholder");
    assert!(h.wait_call_is_backgrounded_for_test(&ToolCallId::from("scheduler-compact")));

    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == "scheduler-compact"
                    && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
        )),
        1,
        "events: {:?}",
        event_log_events(&h)
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&ToolCallId::from("scheduler-compact"))
    );
    let notices_before = event_log_count(&h, |event| matches!(event, Event::HarnessNotice(_)));
    h.emit_info("publication still advances after scheduler compact");
    assert_eq!(
        event_log_count(&h, |event| matches!(event, Event::HarnessNotice(_))),
        notices_before + 1
    );
}

/// The real scheduler path for `agent_compact` likewise publishes exactly one
/// provider placeholder and cannot wedge a subsequent unrelated publication.
#[test]
fn scheduler_agent_compact_publishes_one_placeholder_and_keeps_publication_live() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    install_scheduler_compaction_tools(&mut h);
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role")
        .enable_tools
        .push(ToolName::new("agent_compact"));
    let caller_cid = ensure_test_user_agent(&mut h);
    let target_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let caller_id = h.agent_runtime.agent_registry.agents[&caller_cid]
        .identity
        .agent_id
        .clone()
        .expect("caller id");
    let target_id = h.agent_runtime.agent_registry.agents[&target_cid]
        .identity
        .agent_id
        .clone()
        .expect("target id");
    let prompt_id: AgentPromptId = test_agent_prompt_id("sp-scheduler-agent-compact");
    seed_agent_thinking(&mut h, &caller_cid, prompt_id.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(prompt_id.clone(), caller_cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: prompt_id,
        agent_id: crate::parse_agent_id(&caller_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "scheduler-agent-compact".into(),
            name: ToolName::new("agent_compact"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("agent_id".into()),
                CborValue::Text(target_id),
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
    .expect("scheduler dispatch");

    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == "scheduler-agent-compact"
                    && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
        )),
        1
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&ToolCallId::from("scheduler-agent-compact"))
    );
    let notices_before = event_log_count(&h, |event| matches!(event, Event::HarnessNotice(_)));
    h.emit_info("publication still advances after scheduler agent compact");
    assert_eq!(
        event_log_count(&h, |event| matches!(event, Event::HarnessNotice(_))),
        notices_before + 1
    );
}

/// Cancelling an accepted self request before its safe boundary records one
/// durable pre-start cancellation, consumes its background error, and resumes
/// once with direct bounded error correlation.
#[test]
fn manual_self_compaction_pre_start_cancel_delivers_after_round_closes() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = echo_harness(&state).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    seed_assistant_tool_round(
        &mut h,
        &cid,
        &[
            ("call-cancel-compact", "compact"),
            ("call-cancel-sibling", "sibling"),
        ],
    );
    for (call_id, name) in [
        ("call-cancel-compact", "compact"),
        ("call-cancel-sibling", "sibling"),
    ] {
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert(call_id.into(), cid.clone());
        h.tool_routing.tool_runtime.pending_tools.insert(
            call_id.into(),
            PendingTool {
                name: ToolName::new(name),
                internal_name: ToolName::new(name),
                tool_type: tau_proto::ToolType::Function,
                allows_provider_image: false,
            },
        );
        h.tool_routing
            .tool_runtime
            .tool_turn
            .record_unqueued_in_flight(cid.clone(), call_id.into(), ToolTurnCategories::default());
        h.prompt_coordination
            .prompt_runtime
            .tool_call_prompts
            .insert(call_id.into(), test_agent_prompt_id("sp-seeded-tools"));
    }
    let call = AgentToolCall {
        call_ref: None,
        id: "call-cancel-compact".into(),
        name: ToolName::new("compact"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    };
    h.request_agent_tool_compaction(&cid, &call, ToolName::new("compact"), None);
    h.drain_publish_idle_dispatches();
    let request_id = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentManualCompactionRequested(requested) => Some(requested.request_id),
            _ => None,
        })
        .expect("request correlation");
    h.cancel_remaining_tool_calls(
        &cid,
        vec![call.id.clone()],
        BackgroundCompletionPromptMode::QueuePassive,
    );

    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentManualCompactionRequestFailed(failed)
                    if failed.reason
                        == tau_proto::ManualCompactionRequestFailureReason::Cancelled
            ))
            .count(),
        1
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::ToolBackgroundError(error) if error.call_id == call.id
            ))
            .count(),
        1
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentInferenceDispatchStarted(_)
    )));
    let _interceptor = connect_test_tool(&mut h, "pre-start-delivery-interceptor");
    h.handle_extension_event(
        "pre-start-delivery-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_STEERED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register delivery interceptor");
    h.finish_prebuilt_internal_tool_result(ToolResult {
        presentation: Default::default(),
        call_id: "call-cancel-sibling".into(),
        tool_name: ToolName::new("sibling"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("done".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
            self_compaction_terminal: Some(tau_proto::SelfCompactionTerminal {
                outcome: tau_proto::SelfCompactionTerminalOutcome::RequestFailed {
                    reason: tau_proto::ManualCompactionRequestFailureReason::Cancelled,
                },
                ..
            }),
            ..
        }))
    ));
    let public_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");
    drop(h);
    wait_for_session_unlock(&state, "s1");
    let resumed =
        echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let resumed_cid = resumed
        .runtime_agent_id_for_target_agent(Some(&public_id))
        .expect("resumed agent");
    assert!(!resumed.wait_completion_is_retained_for_test(&resumed_cid, &call.id));
    let assert_restored = |h: &Harness| {
        let records = h
            .session_runtime
            .agent_store
            .agent_events(&public_id)
            .expect("records");
        let terminals = records
            .iter()
            .filter_map(|record| match &record.event {
                Event::AgentPromptSteered(steered) => steered.self_compaction_terminal.as_ref(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            terminals.as_slice(),
            [tau_proto::SelfCompactionTerminal {
                request_id: delivered_request,
                tool_call_id,
                transaction_id: None,
                outcome: tau_proto::SelfCompactionTerminalOutcome::RequestFailed {
                    reason: tau_proto::ManualCompactionRequestFailureReason::Cancelled,
                },
            }] if *delivered_request == request_id && *tool_call_id == call.id
        ));
        assert!(!records.iter().any(|record| matches!(
            &record.event,
            Event::AgentPromptSteered(steered)
                if steered.text == background_completion_prompt(&call.id)
        )));
    };
    assert_restored(&resumed);
    drop(resumed);
    wait_for_session_unlock(&state, "s1");
    let reopened =
        echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
            .expect("second resume");
    assert_restored(&reopened);
    let reopened_cid = reopened
        .runtime_agent_id_for_target_agent(Some(&public_id))
        .expect("reopened agent");
    assert!(!reopened.wait_completion_is_retained_for_test(&reopened_cid, &call.id));
}

/// A started self-compaction failure directly resumes with its typed bounded
/// error and consumes the original wait result.
#[test]
fn manual_self_compaction_failure_delivers_error_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = echo_harness(&state).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let call_id = ToolCallId::from("call-failed-self-compact");
    seed_assistant_tool_round(&mut h, &cid, &[(call_id.as_str(), "compact")]);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("compact"),
            internal_name: ToolName::new("compact"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(cid.clone(), call_id.clone(), ToolTurnCategories::default());
    h.prompt_coordination
        .prompt_runtime
        .tool_call_prompts
        .insert(call_id.clone(), test_agent_prompt_id("sp-seeded-tools"));
    h.request_agent_tool_compaction(
        &cid,
        &AgentToolCall {
            call_ref: None,
            id: call_id.clone(),
            name: ToolName::new("compact"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        },
        ToolName::new("compact"),
        None,
    );
    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .expect("started self compaction");
    let transaction_id = started.transaction_id.clone();
    let _interceptor = connect_test_tool(&mut h, "failed-self-checkpoint-interceptor");
    h.handle_extension_event(
        "failed-self-checkpoint-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register checkpoint interceptor");
    h.publish_for_agent(
        &cid,
        Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
            agent_id: started.agent_id,
            transaction_id: transaction_id.clone(),
            cut: started.cut,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: started.resume_through,
            context_retreat: None,
            incomplete_response: None,
        }),
    );
    let deliveries = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentPromptSteered(steered) => steered.self_compaction_terminal,
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        deliveries.as_slice(),
        [tau_proto::SelfCompactionTerminal {
            tool_call_id,
            transaction_id: Some(delivered_transaction),
            outcome: tau_proto::SelfCompactionTerminalOutcome::Failed {
                reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            },
            ..
        }] if tool_call_id == &call_id && delivered_transaction == &transaction_id
    ));
    assert!(!h.wait_completion_is_retained_for_test(&cid, &call_id));
    assert!(matches!(
        h.runtime_io.publication.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::AgentInferenceDispatchStarted(started))
            if started.transaction_id.is_none()
    ));
    let public_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");
    drop(h);
    wait_for_session_unlock(&state, "s1");
    let resumed =
        echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let records = resumed
        .session_runtime
        .agent_store
        .agent_events(&public_id)
        .expect("restored records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentPromptSteered(steered)
                    if steered.self_compaction_terminal.is_some()
            ))
            .count(),
        1
    );
    let resumed_cid = resumed
        .runtime_agent_id_for_target_agent(Some(&public_id))
        .expect("resumed agent");
    assert!(!resumed.wait_completion_is_retained_for_test(&resumed_cid, &call_id));
    drop(resumed);
    wait_for_session_unlock(&state, "s1");
    let reopened =
        echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
            .expect("second resume");
    let reopened_cid = reopened
        .runtime_agent_id_for_target_agent(Some(&public_id))
        .expect("reopened agent");
    assert!(!reopened.wait_completion_is_retained_for_test(&reopened_cid, &call_id));
}

/// Cold recovery from a committed started failure that crashed before typed
/// delivery reconstructs the actual failure once and resumes it.
#[test]
fn manual_self_compaction_cold_failure_before_delivery() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = echo_harness(&state).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let call_id = ToolCallId::from("call-cold-failed-self-compact");
    seed_assistant_tool_round(&mut h, &cid, &[(call_id.as_str(), "compact")]);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("compact"),
            internal_name: ToolName::new("compact"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(cid.clone(), call_id.clone(), ToolTurnCategories::default());
    h.prompt_coordination
        .prompt_runtime
        .tool_call_prompts
        .insert(call_id.clone(), test_agent_prompt_id("sp-seeded-tools"));
    h.request_agent_tool_compaction(
        &cid,
        &AgentToolCall {
            call_ref: None,
            id: call_id.clone(),
            name: ToolName::new("compact"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        },
        ToolName::new("compact"),
        None,
    );
    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .expect("started self compaction");
    let transaction_id = started.transaction_id.clone();
    let _interceptor = connect_test_tool(&mut h, "failed-self-delivery-interceptor");
    h.handle_extension_event(
        "failed-self-delivery-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_STEERED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register delivery interceptor");
    h.publish_for_agent(
        &cid,
        Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
            agent_id: started.agent_id,
            transaction_id: transaction_id.clone(),
            cut: started.cut,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: started.resume_through,
            context_retreat: None,
            incomplete_response: None,
        }),
    );
    assert!(matches!(
        h.runtime_io.publication.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::AgentPromptSteered(steered))
            if steered.self_compaction_terminal.is_some()
    ));
    let public_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");
    drop(h);
    wait_for_session_unlock(&state, "s1");
    let resumed =
        echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let records = resumed
        .session_runtime
        .agent_store
        .agent_events(&public_id)
        .expect("records");
    let terminals = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentPromptSteered(steered) => steered.self_compaction_terminal.as_ref(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        terminals.as_slice(),
        [tau_proto::SelfCompactionTerminal {
            tool_call_id,
            transaction_id: Some(delivered_transaction),
            outcome: tau_proto::SelfCompactionTerminalOutcome::Failed {
                reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            },
            ..
        }] if *tool_call_id == call_id && *delivered_transaction == transaction_id
    ));
    assert!(!records.iter().any(|record| matches!(
        &record.event,
        Event::AgentPromptSteered(steered)
            if steered.text == background_completion_prompt(&call_id)
    )));
    let resumed_cid = resumed
        .runtime_agent_id_for_target_agent(Some(&public_id))
        .expect("resumed agent");
    assert!(!resumed.wait_completion_is_retained_for_test(&resumed_cid, &call_id));
}

/// Live self-compaction success resumes from the replacement window with one
/// typed terminal and no retained wait result or generic notice.
#[test]
fn manual_self_compaction_success_delivers_directly() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let call_id = ToolCallId::from("call-success-self-compact");
    seed_assistant_tool_round(&mut h, &cid, &[(call_id.as_str(), "compact")]);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("compact"),
            internal_name: ToolName::new("compact"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(cid.clone(), call_id.clone(), ToolTurnCategories::default());
    h.prompt_coordination
        .prompt_runtime
        .tool_call_prompts
        .insert(call_id.clone(), test_agent_prompt_id("sp-seeded-tools"));
    h.request_agent_tool_compaction(
        &cid,
        &AgentToolCall {
            call_ref: None,
            id: call_id.clone(),
            name: ToolName::new("compact"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        },
        ToolName::new("compact"),
        None,
    );
    let compact_prompt = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(provider_text_response(
        &compact_prompt.agent_prompt_id,
        compact_prompt.agent_id,
        "compacted summary",
    ))
    .expect("accept compaction");
    let events = event_log_events(&h);
    let deliveries = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentPromptSteered(steered) => steered.self_compaction_terminal.as_ref(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        deliveries.as_slice(),
        [tau_proto::SelfCompactionTerminal {
            tool_call_id,
            outcome: tau_proto::SelfCompactionTerminalOutcome::Compacted,
            ..
        }] if tool_call_id == &call_id
    ));
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.text == background_completion_prompt(&call_id)
    )));
    assert!(!h.wait_completion_is_retained_for_test(&cid, &call_id));
    let inference = read_nth_prompt_created(&h, 1);
    assert_eq!(inference.operation, tau_proto::PromptOperation::Inference);
    assert!(
        serde_json::to_string(&inference.context)
            .expect("context")
            .contains("compacted summary")
    );
}

/// Cold recovery from compact-success-before-background-terminal reconstructs
/// the background result exactly once, folds its model-visible notification,
/// and only then checkpoints self continuation.
#[test]
fn manual_self_compaction_replay_repairs_completion_before_checkpoint() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = echo_harness(&state).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    seed_assistant_tool_round(&mut h, &cid, &[("call-replay-compact", "compact")]);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("call-replay-compact".into(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        "call-replay-compact".into(),
        PendingTool {
            name: ToolName::new("compact"),
            internal_name: ToolName::new("compact"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(
            cid.clone(),
            "call-replay-compact".into(),
            ToolTurnCategories::default(),
        );
    h.prompt_coordination
        .prompt_runtime
        .tool_call_prompts
        .insert(
            "call-replay-compact".into(),
            test_agent_prompt_id("sp-seeded-tools"),
        );
    h.request_agent_tool_compaction(
        &cid,
        &AgentToolCall {
            call_ref: None,
            id: "call-replay-compact".into(),
            name: ToolName::new("compact"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        },
        ToolName::new("compact"),
        None,
    );
    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .expect("started");
    let transaction_id = started.transaction_id.clone();
    let request_id = match &started.trigger {
        tau_proto::StandaloneCompactionTrigger::ManualAgentTool { request_id, .. } => {
            request_id.clone()
        }
        _ => panic!("manual tool start"),
    };
    let expected_model = started.model.clone();
    let expected_cut = started.cut;
    let suffix_end = h
        .session_runtime
        .agent_store
        .agent(started.agent_id.as_str())
        .and_then(tau_core::AgentTree::head)
        .map(tau_proto::AgentHead::Node)
        .unwrap_or(tau_proto::AgentHead::Root);
    h.session_runtime
        .agent_store
        .append_agent_event_at(
            started.agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentCompacted(tau_proto::AgentCompacted {
                original_input_tokens: None,
                compaction_output_tokens: None,
                agent_id: started.agent_id.clone(),
                transaction_id: Some(transaction_id.clone()),
                cut: Some(started.cut),
                suffix_end: Some(suffix_end),
                compact_prompt_id: Some(started.compact_prompt_id),
                model: Some(started.model),
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
        .expect("seed compact success without harness reaction");
    drop(h);
    wait_for_session_unlock(&state, "s1");

    let resumed =
        echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let events = event_log_events(&resumed);
    let completion = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::ToolBackgroundResult(result)
                    if result.call_id.as_str() == "call-replay-compact"
            )
        })
        .expect("background completion repaired");
    let checkpoint = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentInferenceDispatchStarted(checkpoint)
                    if checkpoint.transaction_id.as_ref() == Some(&transaction_id)
            )
        })
        .expect("checkpoint repaired");
    let notification_text = self_compaction_terminal_prompt(&tau_proto::SelfCompactionTerminal {
        request_id: request_id.clone(),
        tool_call_id: ToolCallId::from("call-replay-compact"),
        transaction_id: Some(transaction_id.clone()),
        outcome: tau_proto::SelfCompactionTerminalOutcome::Compacted,
    });
    let notification = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentPromptSteered(steered) if steered.text == notification_text
            )
        })
        .expect("model-visible completion notification repaired");
    assert!(completion < checkpoint);
    assert!(notification < checkpoint);
    let checkpoint_event = events
        .iter()
        .find_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.transaction_id.as_ref() == Some(&transaction_id) =>
            {
                Some(started)
            }
            _ => None,
        })
        .expect("checkpoint event");
    assert_eq!(checkpoint_event.model.as_ref(), Some(&expected_model));
    assert_eq!(checkpoint_event.activation_cut, Some(expected_cut));
    assert_eq!(
        checkpoint_event.operation,
        Some(tau_proto::PromptOperation::Inference)
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.agent_prompt_id == checkpoint_event.agent_prompt_id
                && prompt.model == expected_model
                && prompt.operation == tau_proto::PromptOperation::Inference
    )));
    assert!(
        resumed
            .session_runtime
            .agent_store
            .agent(checkpoint_event.agent_id.as_str())
            .expect("resumed caller tree")
            .has_user_input_text_on_branch(
                checkpoint_event.through.as_option(),
                &notification_text
            )
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ToolBackgroundResult(result)
                    if result.call_id.as_str() == "call-replay-compact"
            ))
            .count(),
        1
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.text
                == background_completion_prompt(&ToolCallId::from("call-replay-compact"))
    )));
    let resumed_cid = resumed
        .runtime_agent_id_for_target_agent(Some(checkpoint_event.agent_id.as_str()))
        .expect("resumed caller");
    assert!(!resumed.wait_completion_is_retained_for_test(
        &resumed_cid,
        &ToolCallId::from("call-replay-compact")
    ));
}

/// Cold recovery from a retained manual self-compaction success whose caller
/// background result already committed must preserve that terminal, fold its
/// one internal completion notification, checkpoint continuation before
/// dispatch, and leave a second response-less reopen dispatch-uncertain without
/// appending anything beyond its fresh initialization replacement.
#[test]
fn manual_self_compaction_background_terminal_prefix_checkpoints_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let agent_id = tau_proto::AgentId::parse("manual-completed-prefix").expect("durable agent id");
    let request_id =
        tau_proto::CompactionRequestId::parse("cr-manual-completed-prefix").expect("request id");
    let transaction_id = tau_proto::CompactionTransactionId::parse("ct-manual-completed-prefix")
        .expect("transaction id");
    let initiating_prompt_id = test_agent_prompt_id("ap-manual-completed-prefix-0");
    let compact_prompt_id = test_agent_prompt_id("ap-manual-completed-prefix-1");
    let call_id = ToolCallId::from("call-manual-completed-prefix");
    let tool_name = ToolName::new("compact");
    let model: tau_proto::ModelId = "test/model".into();
    let originating_text = "compact the retained prefix".to_owned();
    let notification_text = self_compaction_terminal_prompt(&tau_proto::SelfCompactionTerminal {
        request_id: request_id.clone(),
        tool_call_id: call_id.clone(),
        transaction_id: Some(transaction_id.clone()),
        outcome: tau_proto::SelfCompactionTerminalOutcome::Compacted,
    });
    let is_correlation = |event: &Event| match event {
        Event::AgentPromptSubmitted(event) => {
            event.agent_id == agent_id && event.text == originating_text
        }
        Event::ProviderResponseFinished(event) => {
            event.agent_prompt_id == initiating_prompt_id
                && event.output_items.iter().any(
                    |item| matches!(item, ContextItem::ToolCall(call) if call.call_id == call_id),
                )
        }
        Event::ProviderToolResult(event) | Event::ToolResult(event) => event.call_id == call_id,
        Event::ProviderToolError(event) | Event::ToolError(event) => event.call_id == call_id,
        Event::ToolCancelled(event) => event.call_id == call_id,
        Event::ToolBackgroundResult(event) => event.call_id == call_id,
        Event::ToolBackgroundError(event) => event.call_id == call_id,
        Event::AgentManualCompactionRequested(event) => event.request_id == request_id,
        Event::AgentManualCompactionRequestFailed(event) => event.request_id == request_id,
        Event::AgentStandaloneCompactionStarted(event) => {
            event.transaction_id == transaction_id
                || matches!(
                    &event.trigger,
                    tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                        request_id: event_request_id,
                        ..
                    } if event_request_id == &request_id
                )
        }
        Event::AgentStandaloneCompactionFailed(event) => event.transaction_id == transaction_id,
        Event::AgentCompacted(event) => event.transaction_id.as_ref() == Some(&transaction_id),
        Event::AgentInferenceDispatchStarted(event) => {
            event.transaction_id.as_ref() == Some(&transaction_id)
                || (event.transaction_id.is_none() && event.agent_prompt_id == initiating_prompt_id)
        }
        Event::AgentPromptStarted(event) => {
            event.agent_prompt_id == compact_prompt_id
                || (event.agent_id == agent_id
                    && event.operation == tau_proto::PromptOperation::Inference)
        }
        Event::AgentPromptSteered(event) => {
            event.agent_id == agent_id && event.text == notification_text
        }
        _ => false,
    };

    seed_agent_loaded(&state, "s1", agent_id.as_str());
    let mut store = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: true,
                agent_id: agent_id.clone(),
                text: originating_text.clone(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: tau_proto::PromptSubmissionSource::HumanUi,
                display_name: None,
                ctx_id: None,
            }),
            tau_proto::UnixMicros::new(2),
        )
        .expect("append originating user prompt");
    let originating_through = store
        .agent(agent_id.as_str())
        .and_then(tau_core::AgentTree::head)
        .map(tau_proto::AgentHead::Node)
        .expect("originating prompt head");
    let originating_checkpoint = tau_proto::AgentInferenceDispatchStarted {
        output_length_continuation: None,
        agent_id: agent_id.clone(),
        transaction_id: None,
        agent_prompt_id: initiating_prompt_id.clone(),
        through: originating_through,
        model: Some(model.clone()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(tau_proto::AgentHead::Root),
    };
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentInferenceDispatchStarted(originating_checkpoint.clone()),
            tau_proto::UnixMicros::new(3),
        )
        .expect("append originating inference checkpoint");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
                model_params: Some(tau_proto::ModelParams::default()),
                outer_turn_id: None,

                agent_prompt_id: initiating_prompt_id.clone(),
                agent_id: agent_id.clone(),
                session_id: test_session_id("s1"),
                model: model.clone(),
                operation: tau_proto::PromptOperation::Inference,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }),
            tau_proto::UnixMicros::new(4),
        )
        .expect("append originating inference materialization");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                automatic_compaction_decision: None,
                output_length_disposition: tau_proto::OutputLengthDisposition::None,
                estimated_api_cost_rates: None,
                estimated_api_cost_increment: None,

                agent_prompt_id: initiating_prompt_id.clone(),
                agent_id: agent_id.clone(),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: call_id.clone(),
                    name: tool_name.clone(),
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
            }),
            tau_proto::UnixMicros::new(5),
        )
        .expect("append initiating tool call");
    let requested_target_head = store
        .agent(agent_id.as_str())
        .and_then(tau_core::AgentTree::head)
        .map(tau_proto::AgentHead::Node)
        .expect("tool-calling assistant head");
    let requested = tau_proto::AgentManualCompactionRequested {
        request_id: request_id.clone(),
        target_agent_id: agent_id.clone(),
        source: tau_proto::ManualCompactionSource::Tool(tau_proto::ManualToolCompactionSource {
            caller_agent_id: agent_id.clone(),
            initiating_agent_prompt_id: initiating_prompt_id.clone(),
            initiating_tool_call_id: call_id.clone(),
            initiating_tool_name: tau_proto::ManualCompactionTool::Compact,
            visible_tool_name: tool_name.clone(),
            resume_inference: true,
        }),
        requested_target_head,
        target_generation: store
            .agent(agent_id.as_str())
            .expect("request target tree")
            .ordinary_inference_generation(),
        model: model.clone(),
    };
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentManualCompactionRequested(requested.clone()),
            tau_proto::UnixMicros::new(6),
        )
        .expect("append accepted manual request");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderToolResult(ToolResult {
                presentation: Default::default(),
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Map(Vec::new()),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
            tau_proto::UnixMicros::new(7),
        )
        .expect("append background placeholder");
    let compact_cut = store
        .agent(agent_id.as_str())
        .and_then(tau_core::AgentTree::head)
        .map(tau_proto::AgentHead::Node)
        .expect("complete tool-round head");
    let started = tau_proto::AgentStandaloneCompactionStarted {
        agent_id: agent_id.clone(),
        transaction_id: transaction_id.clone(),
        compact_prompt_id: compact_prompt_id.clone(),
        cut: compact_cut,
        resume_through: Some(compact_cut),
        model: model.clone(),
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        originator: tau_proto::PromptOriginator::User,
        supersedes: None,
        trigger: tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
            request_id: request_id.clone(),
            caller_agent_id: agent_id.clone(),
            initiating_tool_call_id: call_id.clone(),
        },
    };
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentStandaloneCompactionStarted(started.clone()),
            tau_proto::UnixMicros::new(8),
        )
        .expect("append standalone start");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
                model_params: Some(tau_proto::ModelParams::default()),
                outer_turn_id: None,

                agent_prompt_id: compact_prompt_id.clone(),
                agent_id: agent_id.clone(),
                session_id: test_session_id("s1"),
                model: model.clone(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }),
            tau_proto::UnixMicros::new(9),
        )
        .expect("append standalone provider materialization");
    let compacted = tau_proto::AgentCompacted {
        original_input_tokens: None,
        compaction_output_tokens: None,
        agent_id: agent_id.clone(),
        transaction_id: Some(transaction_id.clone()),
        cut: Some(compact_cut),
        suffix_end: Some(compact_cut),
        compact_prompt_id: Some(compact_prompt_id.clone()),
        model: Some(model.clone()),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "retained compact summary".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
    };
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentCompacted(compacted.clone()),
            tau_proto::UnixMicros::new(10),
        )
        .expect("append standalone success");
    let seeded_through = store
        .agent(agent_id.as_str())
        .and_then(tau_core::AgentTree::head)
        .map(tau_proto::AgentHead::Node)
        .expect("compacted head");
    let background_result = tau_proto::ToolBackgroundResult {
        call_id: call_id.clone(),
        tool_name: tool_name.clone(),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Map(vec![
            (
                CborValue::Text("request_id".into()),
                CborValue::Text(request_id.to_string()),
            ),
            (
                CborValue::Text("status".into()),
                CborValue::Text("compacted".into()),
            ),
            (
                CborValue::Text("target_agent_id".into()),
                CborValue::Text(agent_id.to_string()),
            ),
            (
                CborValue::Text("transaction_id".into()),
                CborValue::Text(transaction_id.to_string()),
            ),
        ]),
        originator: tau_proto::PromptOriginator::User,
        display: None,
    };
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ToolBackgroundResult(background_result.clone()),
            tau_proto::UnixMicros::new(11),
        )
        .expect("append retained caller background result");
    let seeded_records = store
        .agent_events(agent_id.as_str())
        .expect("seeded records");
    let seeded_record_count = seeded_records.len();
    let seeded_correlation_count = seeded_records
        .iter()
        .filter(|record| is_correlation(&record.event))
        .count();
    assert_eq!(seeded_correlation_count, 10);
    assert_eq!(
        requested.target_generation,
        tau_proto::MaterializedPromptGeneration::from_inference_generation(1)
    );
    assert_eq!(
        seeded_records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ToolBackgroundResult(result) if result == &background_result
                )
            })
            .count(),
        1
    );
    assert!(!seeded_records.iter().any(|record| matches!(
        &record.event,
        Event::AgentPromptSteered(steered)
            if steered.agent_id == agent_id && steered.text == notification_text
    )));
    assert!(!seeded_records.iter().any(|record| matches!(
        &record.event,
        Event::AgentInferenceDispatchStarted(checkpoint)
            if checkpoint.transaction_id.as_ref() == Some(&transaction_id)
    )));
    assert!(matches!(
        store
            .agent(agent_id.as_str())
            .expect("seeded agent tree")
            .manual_compaction_recoveries()
            .as_slice(),
        [tau_core::ManualCompactionRecovery::Started {
            requested: durable_request,
            started: durable_start,
            outcome: Some(durable_outcome),
        }] if durable_request == &requested
            && durable_start.as_ref() == &started
            && matches!(
                durable_outcome.as_ref(),
                tau_core::ManualCompactionOutcome::Succeeded(durable_success)
                    if durable_success == &compacted
            )
    ));
    assert!(matches!(
        store
            .agent(agent_id.as_str())
            .and_then(tau_core::AgentTree::standalone_compaction_recovery),
        Some(tau_core::StandaloneCompactionRecovery::AwaitingCheckpoint {
            transaction_id: durable_transaction_id,
            cut,
            model: durable_model,
            through,
            ..
        }) if durable_transaction_id == transaction_id
            && cut == compact_cut
            && durable_model == model
            && through == seeded_through
    ));
    drop(store);

    let mut first =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("first cold reopen");
    assert!(
        first.provider_runtime.model_routes.contains_key(&model),
        "captured inference model must have an observed route"
    );
    let first_records = first
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("first reopened records");
    assert_eq!(
        first_records.len(),
        seeded_record_count + 5,
        "recovery records refreshed initialization and resumed the compact terminal outer turn"
    );
    assert_eq!(
        first_records
            .iter()
            .filter(|record| is_correlation(&record.event))
            .count(),
        seeded_correlation_count + 3
    );
    let correlated_positions = |matches_event: &dyn Fn(&Event) -> bool| {
        first_records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| matches_event(&record.event).then_some(index))
            .collect::<Vec<_>>()
    };
    let submitted_positions = correlated_positions(&|event| {
        matches!(
            event,
            Event::AgentPromptSubmitted(prompt)
                if prompt.agent_id == agent_id && prompt.text == originating_text
        )
    });
    let originating_checkpoint_positions = correlated_positions(&|event| {
        matches!(
            event,
            Event::AgentInferenceDispatchStarted(checkpoint)
                if *checkpoint == originating_checkpoint
        )
    });
    let originating_prompt_positions = correlated_positions(&|event| {
        matches!(
            event,
            Event::AgentPromptStarted(prompt)
                if prompt.agent_prompt_id == initiating_prompt_id
                    && prompt.agent_id == agent_id
                    && prompt.model == model
                    && prompt.operation == tau_proto::PromptOperation::Inference
        )
    });
    let response_positions = correlated_positions(&|event| {
        matches!(
            event,
            Event::ProviderResponseFinished(response)
                if response.agent_prompt_id == initiating_prompt_id
        )
    });
    let request_positions = correlated_positions(&|event| {
        matches!(
            event,
            Event::AgentManualCompactionRequested(event) if *event == requested
        )
    });
    let placeholder_positions = correlated_positions(&|event| {
        matches!(
            event,
            Event::ProviderToolResult(result)
                if result.call_id == call_id
                    && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
        )
    });
    let start_positions = correlated_positions(&|event| {
        matches!(
            event,
            Event::AgentStandaloneCompactionStarted(event) if *event == started
        )
    });
    let compact_prompt_positions = correlated_positions(&|event| {
        matches!(
            event,
            Event::AgentPromptStarted(prompt)
                if prompt.agent_prompt_id == compact_prompt_id
                    && prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
        )
    });
    let success_positions = correlated_positions(&|event| {
        matches!(
            event,
            Event::AgentCompacted(event) if *event == compacted
        )
    });
    let background_positions = correlated_positions(&|event| {
        matches!(
            event,
            Event::ToolBackgroundResult(event) if *event == background_result
        )
    });
    let notifications = first_records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match &record.event {
            Event::AgentPromptSteered(steered)
                if steered.agent_id == agent_id && steered.text == notification_text =>
            {
                Some((index, steered))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let checkpoints = first_records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match &record.event {
            Event::AgentInferenceDispatchStarted(checkpoint)
                if checkpoint.transaction_id.as_ref() == Some(&transaction_id) =>
            {
                Some((index, checkpoint))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(submitted_positions.len(), 1);
    assert_eq!(originating_checkpoint_positions.len(), 1);
    assert_eq!(originating_prompt_positions.len(), 1);
    assert_eq!(response_positions.len(), 1);
    assert_eq!(request_positions.len(), 1);
    assert_eq!(placeholder_positions.len(), 1);
    assert_eq!(start_positions.len(), 1);
    assert_eq!(compact_prompt_positions.len(), 1);
    assert_eq!(success_positions.len(), 1);
    assert_eq!(background_positions.len(), 1);
    assert_eq!(notifications.len(), 1);
    assert_eq!(checkpoints.len(), 1);
    assert!(submitted_positions[0] < originating_checkpoint_positions[0]);
    assert!(originating_checkpoint_positions[0] < originating_prompt_positions[0]);
    assert!(originating_prompt_positions[0] < response_positions[0]);
    assert!(response_positions[0] < request_positions[0]);
    assert!(request_positions[0] < placeholder_positions[0]);
    assert!(placeholder_positions[0] < start_positions[0]);
    assert!(start_positions[0] < compact_prompt_positions[0]);
    assert!(compact_prompt_positions[0] < success_positions[0]);
    assert!(success_positions[0] < background_positions[0]);
    assert!(background_positions[0] < notifications[0].0);
    assert!(notifications[0].0 < checkpoints[0].0);
    assert_eq!(
        notifications[0].1.submission_source,
        tau_proto::PromptSubmissionSource::HarnessInternal
    );
    assert_eq!(
        notifications[0].1.message_class,
        tau_proto::PromptMessageClass::Internal
    );
    assert!(notifications[0].1.inference_activation);
    assert_eq!(notifications[0].1.internal_kind, None);
    assert_eq!(notifications[0].1.ctx_id, None);
    let checkpoint = checkpoints[0].1;
    assert_eq!(checkpoint.agent_id, agent_id);
    assert_eq!(checkpoint.model.as_ref(), Some(&model));
    assert_eq!(
        checkpoint.operation,
        Some(tau_proto::PromptOperation::Inference)
    );
    assert_eq!(checkpoint.activation_cut, Some(compact_cut));
    assert_ne!(checkpoint.agent_prompt_id, initiating_prompt_id);
    assert_ne!(checkpoint.agent_prompt_id, compact_prompt_id);
    let inference_materializations = first_records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match &record.event {
            Event::AgentPromptStarted(prompt)
                if prompt.agent_prompt_id == checkpoint.agent_prompt_id
                    && prompt.agent_id == agent_id
                    && prompt.operation == tau_proto::PromptOperation::Inference =>
            {
                Some((index, prompt))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(inference_materializations.len(), 1);
    assert!(checkpoints[0].0 < inference_materializations[0].0);
    assert_eq!(
        inference_materializations[0].1.agent_prompt_id,
        checkpoint.agent_prompt_id
    );
    assert_eq!(inference_materializations[0].1.model, model);
    let live_inference_prompts = event_log_events(&first)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentPromptCreated(prompt)
                if prompt.agent_prompt_id == checkpoint.agent_prompt_id
                    && prompt.agent_id == agent_id
                    && prompt.model == model
                    && prompt.operation == tau_proto::PromptOperation::Inference =>
            {
                Some(prompt)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(live_inference_prompts.len(), 1);
    assert_eq!(
        context_text_count(
            &live_inference_prompts[0],
            &crate::internal_envelope::frame(&notification_text),
        ),
        1
    );
    assert!(
        first
            .session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("first reopened tree")
            .has_user_input_text_on_branch(checkpoint.through.as_option(), &notification_text)
    );
    assert_eq!(
        first
            .session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("first reopened tree")
            .ordinary_inference_generation(),
        tau_proto::MaterializedPromptGeneration::from_inference_generation(
            requested.target_generation.get() + 1,
        )
    );
    assert!(matches!(
        first
            .session_runtime.agent_store
            .agent(agent_id.as_str())
            .and_then(tau_core::AgentTree::standalone_compaction_recovery),
        Some(tau_core::StandaloneCompactionRecovery::DispatchUncertain(
            ref durable_checkpoint
        )) if durable_checkpoint == checkpoint
    ));

    let durable_checkpoint = checkpoint.clone();
    first.shutdown().expect("first shutdown");
    drop(first);
    assert!(
        !tau_core::session_is_locked(&tau_config::settings::sessions_dir_of(&state), "s1")
            .expect("session lock probe"),
        "joined shutdown must release the session lock"
    );
    let mut second =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("second cold reopen");
    let second_records = second
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("second reopened records");
    assert!(second_records.starts_with(&first_records));
    let [initialization] = &second_records[first_records.len()..] else {
        panic!("cold reopen must append exactly one initialization replacement");
    };
    let previous = first_records
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::AgentInitializationContextSet(context) => Some(context),
            _ => None,
        })
        .expect("first run initialization replacement");
    let Event::AgentInitializationContextSet(current) = &initialization.event else {
        panic!("cold reopen suffix must be an initialization replacement");
    };
    let tau_proto::AgentInitializationContextSet {
        session_id: previous_session_id,
        agent_id: previous_agent_id,
        agent_initialization_id: previous_initialization_id,
        agents_message: previous_agents_message,
        effective_skills: previous_effective_skills,
        agents_files: previous_agents_files,
    } = previous;
    let tau_proto::AgentInitializationContextSet {
        session_id: current_session_id,
        agent_id: current_agent_id,
        agent_initialization_id: current_initialization_id,
        agents_message: current_agents_message,
        effective_skills: current_effective_skills,
        agents_files: current_agents_files,
    } = current;
    assert_eq!(current_session_id, &test_session_id("s1"));
    assert_eq!(*current_agent_id, agent_id);
    assert_ne!(current_initialization_id, previous_initialization_id);
    assert_eq!(current_session_id, previous_session_id);
    assert_eq!(current_agent_id, previous_agent_id);
    assert_eq!(current_agents_message, previous_agents_message);
    assert_eq!(current_effective_skills, previous_effective_skills);
    assert_eq!(current_agents_files, previous_agents_files);
    assert_eq!(
        second_records
            .iter()
            .filter(|record| is_correlation(&record.event))
            .count(),
        first_records
            .iter()
            .filter(|record| is_correlation(&record.event))
            .count()
    );
    assert_eq!(
        second_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ToolBackgroundResult(result) if *result == background_result
            ))
            .count(),
        1
    );
    assert_eq!(
        second_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentPromptSteered(steered)
                    if steered.agent_id == agent_id && steered.text == notification_text
            ))
            .count(),
        1
    );
    assert_eq!(
        second_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentInferenceDispatchStarted(checkpoint)
                    if checkpoint == &durable_checkpoint
            ))
            .count(),
        1
    );
    assert_eq!(
        second_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentPromptStarted(prompt)
                    if prompt.agent_prompt_id == durable_checkpoint.agent_prompt_id
                        && prompt.operation == tau_proto::PromptOperation::Inference
            ))
            .count(),
        1
    );
    assert!(
        !event_log_events(&second)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_))),
        "a response-less second reopen must not send any provider prompt"
    );
    assert!(matches!(
        second
            .session_runtime.agent_store
            .agent(agent_id.as_str())
            .and_then(tau_core::AgentTree::standalone_compaction_recovery),
        Some(tau_core::StandaloneCompactionRecovery::DispatchUncertain(
            ref durable
        )) if durable == &durable_checkpoint
    ));
    let second_cid = second.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
    assert!(matches!(
        second.agent_runtime.agent_registry.agents[&second_cid].dispatch.activation_dispatch,
        crate::agent::ActivationDispatchState::DispatchUncertain {
            owner: crate::agent::InferenceCheckpointOwner::Standalone { ref id },
            ref agent_prompt_id,
            through,
            model: Some(ref durable_model),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(cut),
        } if id == &transaction_id
            && agent_prompt_id == &durable_checkpoint.agent_prompt_id
            && through == durable_checkpoint.through
            && durable_model == &model
            && cut == compact_cut
    ));
    let status = &second.agent_runtime.agent_watch.provider_status[agent_id.as_str()];
    assert_eq!(status.agent_prompt_id, durable_checkpoint.agent_prompt_id);
    assert!(matches!(
        status.state,
        tau_proto::AgentWatchProviderState::DispatchUncertain {
            category: tau_proto::AgentWatchProviderCategory::Compaction
        }
    ));
    second.shutdown().expect("second shutdown");
}

/// Cold recovery of a validated manual-tool start without an outcome must
/// terminalize the target as interrupted, complete the caller's background call
/// with one error, and never resend or repeat either repair.
#[test]
fn manual_cross_compaction_started_prefix_is_interrupted_once_without_redispatch() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let caller_id =
        tau_proto::AgentId::parse("caller-interrupted-prefix").expect("caller agent id");
    let target_id =
        tau_proto::AgentId::parse("target-interrupted-prefix").expect("target agent id");
    let request_id =
        tau_proto::CompactionRequestId::parse("cr-interrupted-prefix").expect("request id");
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-interrupted-prefix").expect("transaction id");
    let initiating_prompt_id = test_agent_prompt_id("ap-caller-interrupted-prefix");
    let compact_prompt_id = test_agent_prompt_id("ap-target-interrupted-prefix-compact");
    let call_id = ToolCallId::from("call-interrupted-prefix");
    let tool_name = ToolName::new("agent_compact");
    let is_target_correlation = |event: &Event| match event {
        Event::AgentManualCompactionRequested(event) => event.request_id == request_id,
        Event::AgentManualCompactionRequestFailed(event) => event.request_id == request_id,
        Event::AgentStandaloneCompactionStarted(event) => {
            event.transaction_id == transaction_id
                || matches!(
                    &event.trigger,
                    tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                        request_id: event_request_id,
                        ..
                    } if event_request_id == &request_id
                )
        }
        Event::AgentStandaloneCompactionFailed(event) => event.transaction_id == transaction_id,
        Event::AgentCompacted(event) => event.transaction_id.as_ref() == Some(&transaction_id),
        Event::AgentInferenceDispatchStarted(event) => {
            event.transaction_id.as_ref() == Some(&transaction_id)
        }
        Event::AgentPromptCreated(event) => event.agent_prompt_id == compact_prompt_id,
        Event::ProviderResponseFinished(event) => event.agent_prompt_id == compact_prompt_id,
        _ => false,
    };
    let is_caller_correlation =
        |event: &Event| match event {
            Event::ProviderResponseFinished(event) => event.agent_prompt_id == initiating_prompt_id
                && event.output_items.iter().any(
                    |item| matches!(item, ContextItem::ToolCall(call) if call.call_id == call_id),
                ),
            Event::ProviderToolResult(event) | Event::ToolResult(event) => event.call_id == call_id,
            Event::ProviderToolError(event) | Event::ToolError(event) => event.call_id == call_id,
            Event::ToolCancelled(event) => event.call_id == call_id,
            Event::ToolBackgroundResult(event) => event.call_id == call_id,
            Event::ToolBackgroundError(event) => event.call_id == call_id,
            _ => false,
        };

    seed_agent_loaded(&state, "s1", caller_id.as_str());
    seed_agent_loaded(&state, "s1", target_id.as_str());
    let mut store = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
    store
        .append_agent_event_at(
            caller_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                automatic_compaction_decision: None,
                output_length_disposition: tau_proto::OutputLengthDisposition::None,
                estimated_api_cost_rates: None,
                estimated_api_cost_increment: None,

                agent_prompt_id: initiating_prompt_id.clone(),
                agent_id: caller_id.clone(),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: call_id.clone(),
                    name: tool_name.clone(),
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
            }),
            tau_proto::UnixMicros::new(2),
        )
        .expect("append initiating tool call");
    store
        .append_agent_event_at(
            caller_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderToolResult(ToolResult {
                presentation: Default::default(),
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Map(Vec::new()),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
            tau_proto::UnixMicros::new(3),
        )
        .expect("append background placeholder");
    let requested = tau_proto::AgentManualCompactionRequested {
        request_id: request_id.clone(),
        target_agent_id: target_id.clone(),
        source: tau_proto::ManualCompactionSource::Tool(tau_proto::ManualToolCompactionSource {
            caller_agent_id: caller_id.clone(),
            initiating_agent_prompt_id: initiating_prompt_id.clone(),
            initiating_tool_call_id: call_id.clone(),
            initiating_tool_name: tau_proto::ManualCompactionTool::AgentCompact,
            visible_tool_name: tool_name.clone(),
            resume_inference: false,
        }),
        requested_target_head: tau_proto::AgentHead::Root,
        target_generation: tau_proto::MaterializedPromptGeneration::from_inference_generation(0),
        model: "strict/model".into(),
    };
    store
        .append_agent_event_at(
            target_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentManualCompactionRequested(requested.clone()),
            tau_proto::UnixMicros::new(4),
        )
        .expect("append manual request");
    let started = tau_proto::AgentStandaloneCompactionStarted {
        agent_id: target_id.clone(),
        transaction_id: transaction_id.clone(),
        compact_prompt_id: compact_prompt_id.clone(),
        cut: tau_proto::AgentHead::Root,
        resume_through: None,
        model: "strict/model".into(),
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        originator: tau_proto::PromptOriginator::User,
        supersedes: None,
        trigger: tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
            request_id: request_id.clone(),
            caller_agent_id: caller_id.clone(),
            initiating_tool_call_id: call_id.clone(),
        },
    };
    store
        .append_agent_event_at(
            target_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentStandaloneCompactionStarted(started.clone()),
            tau_proto::UnixMicros::new(5),
        )
        .expect("append standalone start");
    assert!(matches!(
        store
            .agent(target_id.as_str())
            .and_then(tau_core::AgentTree::standalone_compaction_recovery),
        Some(tau_core::StandaloneCompactionRecovery::Interrupted(ref durable_start))
            if durable_start == &started
    ));
    assert!(matches!(
        store
            .agent(target_id.as_str())
            .expect("target tree")
            .manual_compaction_recoveries()
            .as_slice(),
        [tau_core::ManualCompactionRecovery::Started {
            requested: durable_request,
            started: durable_start,
            outcome: None,
        }] if durable_request == &requested && durable_start.as_ref() == &started
    ));
    let seeded_caller_correlation_count = store
        .agent_events(caller_id.as_str())
        .expect("seeded caller records")
        .iter()
        .filter(|record| is_caller_correlation(&record.event))
        .count();
    let seeded_target_correlation_count = store
        .agent_events(target_id.as_str())
        .expect("seeded target records")
        .iter()
        .filter(|record| is_target_correlation(&record.event))
        .count();
    drop(store);

    let mut first = strict_compaction_provider_harness_with_start_reason(
        &state,
        tau_proto::SessionStartReason::Resume,
    )
    .expect("first cold reopen");
    assert!(
        first.provider_runtime.model_info[&"strict/model".into()].supports_standalone_compaction
    );
    assert!(
        first
            .provider_runtime
            .model_routes
            .contains_key(&"strict/model".into())
    );
    let target_records = first
        .session_runtime
        .agent_store
        .agent_events(target_id.as_str())
        .expect("target records");
    assert_eq!(
        target_records
            .iter()
            .filter(|record| is_target_correlation(&record.event))
            .count(),
        seeded_target_correlation_count + 1
    );
    let request_positions = target_records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(
                &record.event,
                Event::AgentManualCompactionRequested(event) if *event == requested
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let start_positions = target_records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(
                &record.event,
                Event::AgentStandaloneCompactionStarted(event) if *event == started
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let failures = target_records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match &record.event {
            Event::AgentStandaloneCompactionFailed(failed)
                if failed.transaction_id == transaction_id =>
            {
                Some((index, failed))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(request_positions.len(), 1);
    assert_eq!(start_positions.len(), 1);
    assert_eq!(failures.len(), 1);
    assert!(request_positions[0] < start_positions[0]);
    assert!(start_positions[0] < failures[0].0);
    assert_eq!(failures[0].1.agent_id, target_id);
    assert_eq!(failures[0].1.cut, started.cut);
    assert_eq!(failures[0].1.resume_through, started.resume_through);
    assert_eq!(
        failures[0].1.reason,
        tau_proto::StandaloneCompactionFailureReason::Interrupted
    );

    let caller_records = first
        .session_runtime
        .agent_store
        .agent_events(caller_id.as_str())
        .expect("caller records");
    assert_eq!(
        caller_records
            .iter()
            .filter(|record| is_caller_correlation(&record.event))
            .count(),
        seeded_caller_correlation_count + 1
    );
    let placeholder_positions = caller_records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(
                &record.event,
                Event::ProviderToolResult(result)
                    if result.call_id == call_id
                        && result.tool_name == tool_name
                        && result.tool_type == tau_proto::ToolType::Function
                        && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let background_errors = caller_records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match &record.event {
            Event::ToolBackgroundError(error) if error.call_id == call_id => Some((index, error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(placeholder_positions.len(), 1);
    assert_eq!(background_errors.len(), 1);
    assert!(placeholder_positions[0] < background_errors[0].0);
    assert_eq!(background_errors[0].1.tool_name, tool_name);
    assert_eq!(
        background_errors[0].1.tool_type,
        tau_proto::ToolType::Function
    );
    assert!(matches!(
        first
            .session_runtime.agent_store
            .agent(target_id.as_str())
            .and_then(tau_core::AgentTree::standalone_compaction_recovery),
        Some(tau_core::StandaloneCompactionRecovery::Blocked {
            failed,
            compact_prompt_id: durable_prompt_id,
        }) if failed == *failures[0].1 && durable_prompt_id == compact_prompt_id
    ));
    assert!(!event_log_events(&first).iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
    )));

    first.shutdown().expect("first shutdown");
    drop(first);
    assert!(
        !tau_core::session_is_locked(&tau_config::settings::sessions_dir_of(&state), "s1")
            .expect("session lock probe"),
        "joined shutdown must release the session lock"
    );
    let mut second = strict_compaction_provider_harness_with_start_reason(
        &state,
        tau_proto::SessionStartReason::Resume,
    )
    .expect("second cold reopen");
    assert!(
        second.provider_runtime.model_info[&"strict/model".into()].supports_standalone_compaction
    );
    assert!(
        second
            .provider_runtime
            .model_routes
            .contains_key(&"strict/model".into())
    );
    let second_target_records = second
        .session_runtime
        .agent_store
        .agent_events(target_id.as_str())
        .expect("second target records");
    assert_eq!(
        second_target_records
            .iter()
            .filter(|record| is_target_correlation(&record.event))
            .count(),
        target_records
            .iter()
            .filter(|record| is_target_correlation(&record.event))
            .count()
    );
    assert_eq!(
        second_target_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentManualCompactionRequested(event) if event.request_id == request_id
            ))
            .count(),
        1
    );
    assert_eq!(
        second_target_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentStandaloneCompactionStarted(event)
                    if event.transaction_id == transaction_id
            ))
            .count(),
        1
    );
    assert_eq!(
        second_target_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentStandaloneCompactionFailed(event)
                    if event.transaction_id == transaction_id
            ))
            .count(),
        1
    );
    let second_caller_records = second
        .session_runtime
        .agent_store
        .agent_events(caller_id.as_str())
        .expect("second caller records");
    assert_eq!(
        second_caller_records
            .iter()
            .filter(|record| is_caller_correlation(&record.event))
            .count(),
        caller_records
            .iter()
            .filter(|record| is_caller_correlation(&record.event))
            .count()
    );
    assert_eq!(
        second_caller_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ToolBackgroundError(error) if error.call_id == call_id
            ))
            .count(),
        1
    );
    assert!(
        !event_log_events(&second)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );
    let second_target_tree = second
        .session_runtime
        .agent_store
        .load_agent(target_id.as_str())
        .expect("load second target")
        .expect("second target tree");
    assert!(matches!(
        second_target_tree.standalone_compaction_recovery(),
        Some(tau_core::StandaloneCompactionRecovery::Blocked { ref failed, .. })
            if failed.transaction_id == transaction_id
                && failed.reason
                    == tau_proto::StandaloneCompactionFailureReason::Interrupted
    ));
    second.shutdown().expect("second shutdown");
}

/// A failed cross-agent manual transaction remains explicitly recoverable in
/// the same ordinary generation without an intervening automatic retry.
#[test]
fn manual_cross_compaction_recovers_failed_tool_at_same_generation() {
    assert_failed_manual_tool_recovery(false);
}

/// Cold replay preserves a failed cross-agent manual transaction and its first
/// background error, then permits exactly one same-generation successor.
#[test]
fn manual_cross_compaction_cold_replay_recovers_failed_tool_at_same_generation() {
    assert_failed_manual_tool_recovery(true);
}

/// Explicit recovery requires exact durable provider-qualified model and safe
/// current-head ancestry.
#[test]
fn manual_cross_compaction_recovery_requires_exact_failure_authority() {
    let (_td, mut h, _caller, target, _call, target_id) = setup_manual_cross_compaction_test();
    let (prefix, _, _) = seed_historical_open_prefix_failure(&mut h, &target, "echo/model");
    let current_head = h.agent_runtime.agent_registry.agents[&target]
        .identity
        .head
        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
    assert!(
        h.matching_durable_failed_recovery(target_id.as_str(), &"echo/model".into(), current_head)
            .is_some()
    );
    assert!(
        h.matching_durable_failed_recovery(target_id.as_str(), &"other/model".into(), current_head)
            .is_none()
    );
    assert!(
        h.matching_durable_failed_recovery(target_id.as_str(), &"echo/model".into(), prefix)
            .is_none(),
        "a sibling or retreated current head must not satisfy the owed resume branch"
    );
    h.shutdown().expect("shutdown");
}

/// Model self-`compact` can explicitly retry a matching failed transaction.
#[test]
fn manual_self_compaction_retries_matching_failed_transaction() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    seed_historical_open_prefix_failure(&mut h, &cid, "echo/model");
    let current_head = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
    assert!(
        h.matching_durable_failed_recovery(agent_id.as_str(), &"echo/model".into(), current_head)
            .is_some()
    );

    let historical = tau_proto::AgentManualCompactionRequested {
        request_id: tau_proto::CompactionRequestId::parse("cr-self-historical")
            .expect("request id"),
        target_agent_id: agent_id.clone(),
        source: tau_proto::ManualCompactionSource::Tool(tau_proto::ManualToolCompactionSource {
            caller_agent_id: agent_id.clone(),
            initiating_agent_prompt_id: test_agent_prompt_id("ap-self-historical"),
            initiating_tool_call_id: "call-self-historical".into(),
            initiating_tool_name: tau_proto::ManualCompactionTool::Compact,
            visible_tool_name: ToolName::new("compact"),
            resume_inference: true,
        }),
        requested_target_head: current_head,
        target_generation: tau_proto::MaterializedPromptGeneration::from_inference_generation(0),
        model: "echo/model".into(),
    };
    h.publish_for_agent(
        &cid,
        Event::AgentManualCompactionRequested(historical.clone()),
    );
    h.publish_for_agent(
        &cid,
        Event::AgentManualCompactionRequestFailed(tau_proto::AgentManualCompactionRequestFailed {
            request_id: historical.request_id,
            target_agent_id: agent_id.clone(),
            reason: tau_proto::ManualCompactionRequestFailureReason::Cancelled,
        }),
    );

    seed_assistant_tool_round(&mut h, &cid, &[("call-self-blocked", "compact")]);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("call-self-blocked".into(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        "call-self-blocked".into(),
        PendingTool {
            name: ToolName::new("compact"),
            internal_name: ToolName::new("compact"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(
            cid.clone(),
            "call-self-blocked".into(),
            ToolTurnCategories::default(),
        );
    h.prompt_coordination
        .prompt_runtime
        .tool_call_prompts
        .insert(
            "call-self-blocked".into(),
            test_agent_prompt_id("sp-seeded-tools"),
        );
    let call = AgentToolCall {
        call_ref: None,
        id: "call-self-blocked".into(),
        name: ToolName::new("compact"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    };
    h.request_agent_tool_compaction(&cid, &call, ToolName::new("compact"), None);
    h.drain_publish_idle_dispatches();
    h.try_advance_queue();
    h.drain_publish_idle_dispatches();
    h.try_advance_queue();

    assert_eq!(
        h.session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("agent tree")
            .manual_compaction_recoveries()
            .len(),
        2
    );
    assert_eq!(durable_compaction_counts(&h, &agent_id), (2, 2, 1, 0));
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&call.id)
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::Running { .. }
    ));
    h.shutdown().expect("shutdown");
}

/// Successful cross-agent manual compaction still consumes its ordinary
/// generation and rejects another request before any ordinary inference.
#[test]
fn manual_cross_compaction_successful_repeat_at_same_generation_is_not_needed() {
    let (_td, mut h, caller, target, first_call, target_id) = setup_manual_cross_compaction_test();
    h.publish_for_agent(
        &target,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: target_id.clone(),
            text: "content to compact".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    h.request_agent_tool_compaction(
        &caller,
        &first_call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .expect("first transaction");
    let suffix_end = h
        .session_runtime
        .agent_store
        .agent(target_id.as_str())
        .and_then(tau_core::AgentTree::head)
        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
    h.publish_for_agent(
        &target,
        Event::AgentCompacted(tau_proto::AgentCompacted {
            original_input_tokens: None,
            compaction_output_tokens: None,
            agent_id: target_id.clone(),
            transaction_id: Some(started.transaction_id),
            cut: Some(started.cut),
            suffix_end: Some(suffix_end),
            compact_prompt_id: Some(started.compact_prompt_id),
            model: Some(started.model),
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
    );
    assert_eq!(
        durable_compaction_counts(&h, &target_id),
        (1, 1, 0, 1),
        "{:?}",
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .find(|agent| agent.identity.agent_id.as_deref() == Some(target_id.as_str()))
            .map(|agent| (&agent.turn.turn_state, &agent.dispatch.activation_dispatch))
    );

    let repeated_call =
        register_manual_cross_compaction_call(&mut h, &caller, "call-successful-repeat");
    h.request_agent_tool_compaction(
        &caller,
        &repeated_call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    assert!(
        event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::ToolError(error)
                if error.call_id == repeated_call.id && error.message == "not_needed"
        )),
        "{:?}",
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(
                event,
                Event::ToolError(error) if error.call_id == repeated_call.id
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(durable_compaction_counts(&h, &target_id), (1, 1, 0, 1));
    h.shutdown().expect("shutdown");
}

/// Regression: a completed manual request at generation zero must not reject a
/// later request after the target completes another ordinary inference.
#[test]
fn manual_compaction_accepts_later_inference_generation() {
    let (_td, mut h, caller, target, call, target_id) = setup_manual_cross_compaction_test();
    let historical = tau_proto::AgentManualCompactionRequested {
        request_id: tau_proto::CompactionRequestId::parse("cr-historical").expect("request id"),
        target_agent_id: target_id.clone(),
        source: tau_proto::ManualCompactionSource::Tool(tau_proto::ManualToolCompactionSource {
            caller_agent_id: durable_agent_id_for_conversation(&h, &caller),
            initiating_agent_prompt_id: test_agent_prompt_id("ap-older-caller-turn"),
            initiating_tool_call_id: "call-historical".into(),
            initiating_tool_name: tau_proto::ManualCompactionTool::AgentCompact,
            visible_tool_name: ToolName::new("agent_compact"),
            resume_inference: false,
        }),
        requested_target_head: tau_proto::AgentHead::Root,
        target_generation: tau_proto::MaterializedPromptGeneration::from_inference_generation(0),
        model: "echo/model".into(),
    };
    h.publish_for_agent(
        &target,
        Event::AgentManualCompactionRequested(historical.clone()),
    );
    h.publish_for_agent(
        &target,
        Event::AgentManualCompactionRequestFailed(tau_proto::AgentManualCompactionRequestFailed {
            request_id: historical.request_id,
            target_agent_id: historical.target_agent_id,
            reason: tau_proto::ManualCompactionRequestFailureReason::Cancelled,
        }),
    );

    h.dispatch_prompt_for_agent(&target, PendingPrompt::user("new target work".to_owned()))
        .expect("dispatch later ordinary inference");
    let prompt_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&target)
        .and_then(|agent| agent.dispatch.in_flight_prompt.clone())
        .expect("later ordinary provider prompt");
    h.handle_provider_response_finished(provider_text_response(
        &prompt_id,
        target_id.clone(),
        "new target work",
    ))
    .expect("finish later ordinary inference");
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent(target_id.as_str())
            .expect("target tree")
            .ordinary_inference_generation(),
        tau_proto::MaterializedPromptGeneration::from_inference_generation(1)
    );

    h.request_agent_tool_compaction(
        &caller,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );

    assert!(event_log_events(&h).into_iter().any(|event| matches!(
        event,
        Event::AgentManualCompactionRequested(request)
            if request.required_tool_source().initiating_tool_call_id == call.id && request.target_generation == tau_proto::MaterializedPromptGeneration::from_inference_generation(1)
    )));
    assert!(event_log_events(&h).into_iter().any(|event| matches!(
        event,
        Event::AgentStandaloneCompactionStarted(started)
            if started.agent_id == target_id
    )));
    assert!(!event_log_events(&h).into_iter().any(|event| matches!(
        event,
        Event::ToolError(error) if error.call_id == call.id && error.message == "not_needed"
    )));
    h.shutdown().expect("shutdown");
}

/// Successful manual-compaction acceptance and start remain target-owned typed
/// facts, while a later transaction failure still completes through the
/// canonical background-error path.
#[test]
fn manual_compaction_lifecycle_distinguishes_status_from_failure() {
    let (_td, mut h, caller_cid, target_cid, call, target_id) =
        setup_manual_cross_compaction_test();
    h.request_agent_tool_compaction(
        &caller_cid,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );

    let events = event_log_events(&h);
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentManualCompactionRequested(requested)
            if requested.target_agent_id == target_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentStandaloneCompactionStarted(started)
            if started.agent_id == target_id
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.message.contains("accepted compaction request")
                || notice.message.contains("Starting compaction request")
    )));

    let started = events
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) if started.agent_id == target_id => {
                Some(started)
            }
            _ => None,
        })
        .expect("compaction transaction started");
    h.publish_for_agent(
        &target_cid,
        Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
            agent_id: started.agent_id,
            transaction_id: started.transaction_id,
            cut: started.cut,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: started.resume_through,
            context_retreat: None,
            incomplete_response: None,
        }),
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error) if error.call_id == call.id
    )));
    h.shutdown().expect("shutdown");
}

/// Model-tool acceptance and its successor start each retain their exact
/// target-owned parent across append rejection; acknowledgement and successor
/// runtime ownership install only after the corresponding durable commit.
#[test]
fn model_compaction_acceptance_and_start_commit_before_runtime_installation() {
    let (_td, mut h, caller_cid, _target_cid, call, target_id) =
        setup_manual_cross_compaction_test();
    let target_cid = h
        .runtime_agent_id_for_target_agent(Some(target_id.as_str()))
        .expect("target route");
    let acceptance_parent = h
        .selected_head_for_agent(&target_cid)
        .unwrap_or(tau_proto::AgentHead::Root);
    connect_test_tool(&mut h, "model-compaction-cuts");
    h.handle_extension_event(
        "model-compaction-cuts",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_MANUAL_COMPACTION_REQUESTED),
                EventSelector::Exact(tau_proto::EventName::AGENT_STANDALONE_COMPACTION_STARTED),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register compaction cut interceptor");
    h.request_agent_tool_compaction(
        &caller_cid,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    let request_key = h
        .prompt_coordination
        .compaction_runtime
        .pending_manual_acceptances
        .keys()
        .next()
        .cloned()
        .expect("staged model acceptance");
    let request_id = request_key.request_id().clone();
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .is_empty()
            && !h
                .tool_routing
                .tool_runtime
                .tool_turn
                .is_backgrounded(&call.id)
    );
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "model-compaction-cuts",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("reject acceptance append");
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .pending_manual_acceptances
            .contains_key(&request_key)
            && h.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .contains_key(&target_cid)
            && !h
                .tool_routing
                .tool_runtime
                .tool_turn
                .is_backgrounded(&call.id)
    );
    h.retry_pending_agent_publish_completion(&target_cid);
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .pending_manual_acceptances
            .is_empty()
            && h.prompt_coordination
                .compaction_runtime
                .accepted_manual_tools
                .contains_key(&request_key)
            && h.prompt_coordination
                .compaction_runtime
                .active_manual_starts_is_empty()
            && h.tool_routing
                .tool_runtime
                .tool_turn
                .is_backgrounded(&call.id),
        "acceptance commit acknowledges before the parked start installs runtime ownership"
    );
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::AgentStandaloneCompactionStarted(_))
    ));
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "model-compaction-cuts",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("reject start append");
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .contains_key(&request_key)
            && h.prompt_coordination
                .compaction_runtime
                .active_manual_starts_is_empty()
            && h.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .contains_key(&target_cid)
    );
    h.retry_pending_agent_publish_completion(&target_cid);

    let records = h
        .session_runtime
        .agent_store
        .agent_events(target_id.as_str())
        .expect("target events");
    let requests = records
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentManualCompactionRequested(request)
                    if request.request_id == request_id
            )
        })
        .collect::<Vec<_>>();
    let starts = records
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        &started.trigger,
                        tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                            request_id: started_request_id,
                            ..
                        } if started_request_id == &request_id
                    )
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].parent,
        tau_core::AgentEventParent::from_head(acceptance_parent)
    );
    assert_eq!(starts.len(), 1);
    assert_eq!(
        starts[0].parent,
        tau_core::AgentEventParent::from_head(acceptance_parent)
    );
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .pending_manual_acceptances
            .is_empty()
            && !h
                .prompt_coordination
                .compaction_runtime
                .accepted_manual_tools
                .contains_key(&request_key)
            && h.prompt_coordination
                .compaction_runtime
                .has_model_tool_start(
                    match &starts[0].event {
                        Event::AgentStandaloneCompactionStarted(started) => {
                            started.agent_id.clone()
                        }
                        _ => unreachable!("filtered start"),
                    },
                    match &starts[0].event {
                        Event::AgentStandaloneCompactionStarted(started) => {
                            started.transaction_id.clone()
                        }
                        _ => unreachable!("filtered start"),
                    },
                )
            && !h
                .prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .contains_key(&target_cid)
    );
}

/// A staged UI acceptance excludes a model-tool acceptance for the same target,
/// preserving the `already_pending` rejection instead of retaining two origins.
#[test]
fn staged_ui_acceptance_excludes_model_acceptance_for_same_target() {
    let (_td, mut h, caller_cid, _target_cid, call, target_id) =
        setup_manual_cross_compaction_test();
    connect_test_tool(&mut h, "cross-origin-acceptance");
    h.handle_extension_event(
        "cross-origin-acceptance",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MANUAL_COMPACTION_REQUESTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register acceptance interceptor");

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(target_id.as_str()),
    );
    h.request_agent_tool_compaction(
        &caller_cid,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    assert!(matches!(
        h.prompt_coordination
            .compaction_runtime
            .pending_manual_acceptances
            .values()
            .next(),
        Some(crate::harness::PendingManualCompactionAcceptance::Ui(_))
    ));

    h.handle_extension_event(
        "cross-origin-acceptance",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit UI acceptance");
    assert_manual_cross_compaction_error(&h, &call, "already_pending");
}

/// A UI request cannot overwrite a staged model-tool acceptance or steal its
/// acknowledgement and transaction correlation.
#[test]
fn staged_model_acceptance_excludes_ui_and_preserves_call_correlation() {
    let (_td, mut h, caller_cid, _target_cid, first_call, target_id) =
        setup_manual_cross_compaction_test();
    let ui_frames =
        connect_test_client(&mut h, "acceptance-collision-ui", tau_proto::ClientKind::Ui);
    connect_test_tool(&mut h, "model-acceptance-double-request");
    h.handle_extension_event(
        "model-acceptance-double-request",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MANUAL_COMPACTION_REQUESTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register acceptance interceptor");
    h.request_agent_tool_compaction(
        &caller_cid,
        &first_call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    h.handle_compact_request(
        &crate::test_connection_id("acceptance-collision-ui"),
        test_session_id("s1"),
        Some(target_id.as_str()),
    );
    let staged = h
        .prompt_coordination
        .compaction_runtime
        .pending_manual_acceptances
        .values()
        .collect::<Vec<_>>();
    assert_eq!(staged.len(), 1);
    assert_eq!(
        staged[0]
            .request()
            .required_tool_source()
            .initiating_tool_call_id,
        first_call.id
    );
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .pending_ui_acknowledgements
            .is_empty(),
        "the rejected UI origin must not acquire ACK delivery"
    );
    assert!(ui_frames.lock().expect("UI frames").iter().any(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::HarnessNotice(notice))
                if notice.message == "compaction already queued"
        )
    }));
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&first_call.id)
    );
    h.handle_extension_event(
        "model-acceptance-double-request",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit first acceptance");

    let events = event_log_events(&h);
    let requests = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentManualCompactionRequested(request) => Some(request),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 1);
    let request_id = requests[0].request_id.clone();
    let starts = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1);
    assert!(matches!(
        &starts[0].trigger,
        tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
            request_id: started_request_id,
            initiating_tool_call_id,
            ..
        } if started_request_id == &request_id && initiating_tool_call_id == &first_call.id
    ));
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&first_call.id)
    );
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .model_tool_start_by_call(&first_call.id)
            .is_some_and(|(_, pending)| pending.request_id == request_id)
    );
}

/// Caller/target teardown and session rollover discard a model-tool acceptance
/// that has not committed, without ACKing or retaining stale quota ownership.
#[test]
fn staged_model_compaction_acceptance_is_cleared_by_teardown() {
    for teardown in ["target_unload", "caller_unload", "session_switch"] {
        let (_td, mut h, caller_cid, target_cid, call, target_id) =
            setup_manual_cross_compaction_test();
        let interceptor = match teardown {
            "target_unload" => "model-acceptance-target-unload",
            "caller_unload" => "model-acceptance-caller-unload",
            "session_switch" => "model-acceptance-session-switch",
            _ => unreachable!("fixed teardown cases"),
        };
        connect_test_tool(&mut h, interceptor);
        h.handle_extension_event(
            interceptor,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_MANUAL_COMPACTION_REQUESTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register acceptance interceptor");
        h.request_agent_tool_compaction(
            &caller_cid,
            &call,
            ToolName::new("agent_compact"),
            Some(&target_id),
        );
        assert_eq!(
            h.prompt_coordination
                .compaction_runtime
                .pending_manual_acceptances
                .len(),
            1
        );
        let request_key = h
            .prompt_coordination
            .compaction_runtime
            .pending_manual_acceptances
            .keys()
            .next()
            .cloned()
            .expect("staged request id");
        let request_id = request_key.request_id().clone();
        if teardown == "caller_unload" {
            reject_next_semantic_admission(&h);
            h.handle_extension_event(
                interceptor,
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(None),
                })),
            )
            .expect("retain append-rejected acceptance");
            assert!(
                h.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .contains_key(&target_cid)
            );
        }
        match teardown {
            "target_unload" => h.remove_agent_expected(&target_cid),
            "caller_unload" => h.remove_agent_expected(&caller_cid),
            "session_switch" => h
                .switch_session(test_session_id("s2"), tau_proto::SessionStartReason::New)
                .expect("switch session"),
            _ => unreachable!("fixed teardown cases"),
        }
        assert!(
            h.prompt_coordination
                .compaction_runtime
                .pending_manual_acceptances
                .is_empty()
                && !h
                    .tool_routing
                    .tool_runtime
                    .tool_turn
                    .is_backgrounded(&call.id),
            "teardown={teardown}"
        );
        assert!(
            h.runtime_io.publication.pending_intercept.is_none()
                && h.runtime_io.publication.deferred.iter().all(|pending| {
                    !matches!(
                        pending.event(),
                        Event::AgentManualCompactionRequested(request)
                            if request.request_id == request_id
                    )
                }),
            "teardown={teardown}"
        );
        assert!(
            h.prompt_coordination
                .compaction_runtime
                .accepted_manual_tools
                .is_empty()
                && h.prompt_coordination
                    .compaction_runtime
                    .active_manual_starts_is_empty()
                && h.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .is_empty(),
            "teardown={teardown}"
        );
    }
}

/// Possession of the cross-agent capability authorizes an unrelated loaded
/// agent without ancestry, watch, or message relationships.
#[test]
fn manual_cross_compaction_starts_for_unrelated_loaded_agent() {
    let (_td, mut h, caller_cid, target_cid, call, target_id) =
        setup_manual_cross_compaction_test();
    h.request_agent_tool_compaction(
        &caller_cid,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );

    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started)
                if started.agent_id.as_str() == "unrelated-target"
                    && matches!(
                        started.trigger,
                        tau_proto::StandaloneCompactionTrigger::ManualAgentTool { .. }
                    ) =>
            {
                Some(started)
            }
            _ => None,
        })
        .expect("unrelated target starts");
    let recoveries = h
        .session_runtime
        .agent_store
        .agent("unrelated-target")
        .expect("target tree")
        .manual_compaction_recoveries();
    assert!(
        matches!(
            recoveries.as_slice(),
            [tau_core::ManualCompactionRecovery::Started { .. }]
        ),
        "{recoveries:?}"
    );
    h.publish_for_agent(
        &target_cid,
        Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
            agent_id: started.agent_id,
            transaction_id: started.transaction_id,
            cut: started.cut,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: started.resume_through,
            context_retreat: None,
            incomplete_response: None,
        }),
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error) if error.call_id == call.id
    )));
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| match event {
                Event::ToolBackgroundResult(result) => result.call_id == call.id,
                Event::ToolBackgroundError(error) => error.call_id == call.id,
                _ => false,
            })
            .count(),
        1,
        "tracked={} backgrounded={} events={:?}",
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&call.id),
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&call.id),
        event_log_events(&h)
            .into_iter()
            .filter(|event| match event {
                Event::ToolResult(result) | Event::ProviderToolResult(result) => {
                    result.call_id == call.id
                }
                Event::ToolError(error) | Event::ProviderToolError(error) => {
                    error.call_id == call.id
                }
                Event::ToolBackgroundResult(result) => result.call_id == call.id,
                Event::ToolBackgroundError(error) => error.call_id == call.id,
                Event::AgentCompacted(_) | Event::AgentStandaloneCompactionFailed(_) => true,
                _ => false,
            })
            .collect::<Vec<_>>()
    );
    h.shutdown().expect("shutdown");
}

/// Authorized `agent_compact` must repair an unrelated loaded target whose
/// historical failed transaction cut through a complete provider tool round.
#[test]
fn manual_cross_compaction_repairs_blocked_open_prefix_target() {
    let (_td, mut h, caller_cid, target_cid, call, target_id) =
        setup_manual_cross_compaction_test();
    let (prefix, assistant, results) =
        seed_historical_open_prefix_failure(&mut h, &target_cid, "echo/model");

    h.request_agent_tool_compaction(
        &caller_cid,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    let starts: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) if started.agent_id == target_id => {
                Some(started)
            }
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0].cut, assistant);
    assert_eq!(starts[0].resume_through, Some(results));
    assert_eq!(starts[1].cut, prefix);
    assert_eq!(starts[1].resume_through, Some(results));
    assert_eq!(
        starts[1].supersedes.as_ref(),
        Some(&starts[0].transaction_id)
    );
    assert!(matches!(
        starts[1].trigger,
        tau_proto::StandaloneCompactionTrigger::ManualAgentTool { .. }
    ));
    let retry = read_nth_prompt_created(&h, 1);
    h.handle_provider_response_finished(provider_text_response(
        &retry.agent_prompt_id,
        retry.agent_id,
        "cross-agent recovered summary",
    ))
    .expect("accept cross-agent recovery");
    assert!(!matches!(
        h.agent_runtime.agent_registry.agents[&target_cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    h.shutdown().expect("shutdown");
}

/// Authorized `agent_compact` starts a fresh transaction when navigation
/// changes the target's failure authority.
#[test]
fn manual_cross_compaction_starts_fresh_after_branch_change() {
    let (_td, mut h, caller_cid, target_cid, call, target_id) =
        setup_manual_cross_compaction_test();
    let (prefix, _, _) = seed_historical_open_prefix_failure(&mut h, &target_cid, "echo/model");
    h.publish_for_agent(
        &target_cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: target_id.clone(),
            head: prefix,
        }),
    );

    h.request_agent_tool_compaction(
        &caller_cid,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionStarted(started)
                    if started.agent_id == target_id
            ))
            .count(),
        2
    );
    assert!(!event_log_events(&h).into_iter().any(|event| matches!(
        event,
        Event::AgentManualCompactionRequestFailed(failed)
            if failed.target_agent_id == target_id
                && failed.reason
                    == tau_proto::ManualCompactionRequestFailureReason::StaleBranch
    )));
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&target_cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::Running { .. }
    ));
    h.shutdown().expect("shutdown");
}

/// Cancelling after a cross-agent transaction starts targets that exact compact
/// prompt and completes the original background call once.
#[test]
fn manual_cross_compaction_post_start_cancel_is_exact() {
    let (_td, mut h, caller, _target, call, target_id) = setup_manual_cross_compaction_test();
    h.request_agent_tool_compaction(
        &caller,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    h.cancel_remaining_tool_calls(
        &caller,
        vec![call.id.clone()],
        BackgroundCompletionPromptMode::QueuePassive,
    );

    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.reason
                        == tau_proto::StandaloneCompactionFailureReason::Cancelled
            ))
            .count(),
        1
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::ToolBackgroundError(error) if error.call_id == call.id
            ))
            .count(),
        1
    );
}

/// A cross-agent request rejects a busy target without persisting acceptance.
#[test]
fn manual_cross_compaction_rejects_busy_target() {
    let (_td, mut h, caller, target, call, target_id) = setup_manual_cross_compaction_test();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&target)
        .expect("target")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("ap-busy"),
    };
    h.request_agent_tool_compaction(
        &caller,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    assert_manual_cross_compaction_error(&h, &call, "target_busy");
}

/// A cross-agent request rejects uncertain dispatch without persisting
/// acceptance.
#[test]
fn manual_cross_compaction_rejects_uncertain_dispatch() {
    let (_td, mut h, caller, target, call, target_id) = setup_manual_cross_compaction_test();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&target)
        .expect("target")
        .dispatch
        .activation_dispatch = path_crate_agent::ActivationDispatchState::DispatchUncertain {
        owner: path_crate_agent::InferenceCheckpointOwner::Inference,
        agent_prompt_id: test_agent_prompt_id("ap-uncertain"),
        through: tau_proto::AgentHead::Root,
        model: Some("echo/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(tau_proto::AgentHead::Root),
    };
    h.request_agent_tool_compaction(
        &caller,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    assert_manual_cross_compaction_error(&h, &call, "dispatch_uncertain");
}

/// A cross-agent request rejects an unsupported model without persisting
/// acceptance.
#[test]
fn manual_cross_compaction_rejects_unsupported_model() {
    let (_td, mut h, caller, _target, call, target_id) = setup_manual_cross_compaction_test();
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = false;
    h.request_agent_tool_compaction(
        &caller,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    assert_manual_cross_compaction_error(&h, &call, "standalone_compaction_unsupported");
}

/// A cross-agent request rejects an unavailable target without revealing its
/// state.
#[test]
fn manual_cross_compaction_rejects_unavailable_target() {
    let (_td, mut h, caller, _target, call, _target_id) = setup_manual_cross_compaction_test();
    let unknown = tau_proto::AgentId::parse("unknown-target").expect("unknown id");
    h.request_agent_tool_compaction(
        &caller,
        &call,
        ToolName::new("agent_compact"),
        Some(&unknown),
    );
    assert_manual_cross_compaction_error(&h, &call, "target_unavailable_or_unauthorized");
}

/// A cross-agent request rejects a caller at its compaction limit without
/// acceptance.
#[test]
fn manual_cross_compaction_rejects_caller_limit() {
    let (_td, mut h, caller, _target, call, target_id) = setup_manual_cross_compaction_test();
    let caller_public = crate::parse_agent_id(
        h.agent_runtime.agent_registry.agents[&caller]
            .identity
            .agent_id
            .as_deref()
            .expect("caller public id"),
    );
    for index in 0..4 {
        h.prompt_coordination
            .compaction_runtime
            .record_model_tool_start(
                tau_proto::AgentId::parse(format!("target-{index}")).expect("target id"),
                tau_proto::CompactionTransactionId::parse(format!("ct-cap-{index}"))
                    .expect("transaction id"),
                crate::harness::PendingManualCompactionTool {
                    request_id: tau_proto::CompactionRequestId::parse(format!("cr-cap-{index}"))
                        .expect("request id"),
                    caller_agent_id: caller_public.clone(),
                    call_id: format!("call-cap-{index}").into(),
                    tool_name: ToolName::new("agent_compact"),
                    target_agent_id: tau_proto::AgentId::parse(format!("target-{index}"))
                        .expect("target id"),
                },
            );
    }
    h.request_agent_tool_compaction(
        &caller,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    assert_manual_cross_compaction_error(&h, &call, "caller_compaction_limit");
}

/// A repeated request and its mismatched block both preserve the repeat guard.
#[test]
fn manual_cross_compaction_rejects_repeat_guard() {
    let (_td, mut h, caller, target, call, target_id) = setup_manual_cross_compaction_test();
    let historical = tau_proto::AgentManualCompactionRequested {
        request_id: tau_proto::CompactionRequestId::parse("cr-historical").expect("request id"),
        target_agent_id: target_id.clone(),
        source: tau_proto::ManualCompactionSource::Tool(tau_proto::ManualToolCompactionSource {
            caller_agent_id: crate::parse_agent_id(
                h.agent_runtime.agent_registry.agents[&caller]
                    .identity
                    .agent_id
                    .as_deref()
                    .expect("caller public id"),
            ),
            initiating_agent_prompt_id: test_agent_prompt_id("ap-older-caller-turn"),
            initiating_tool_call_id: "call-historical".into(),
            initiating_tool_name: tau_proto::ManualCompactionTool::AgentCompact,
            visible_tool_name: ToolName::new("agent_compact"),
            resume_inference: false,
        }),
        requested_target_head: tau_proto::AgentHead::Root,
        target_generation: tau_proto::MaterializedPromptGeneration::from_inference_generation(0),
        model: "echo/model".into(),
    };
    h.publish_for_agent(
        &target,
        Event::AgentManualCompactionRequested(historical.clone()),
    );
    h.publish_for_agent(
        &target,
        Event::AgentManualCompactionRequestFailed(tau_proto::AgentManualCompactionRequestFailed {
            request_id: historical.request_id,
            target_agent_id: historical.target_agent_id,
            reason: tau_proto::ManualCompactionRequestFailureReason::Cancelled,
        }),
    );
    h.request_agent_tool_compaction(
        &caller,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    assert_manual_cross_compaction_error(&h, &call, "not_needed");
}

/// A UI request made during provider inference is durable, coalesces, and
/// claims exactly one standalone start once the target reaches a closed idle
/// boundary.
#[test]
fn busy_ui_compaction_queues_and_claims_once_at_idle_boundary() {
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
    seed_agent_thinking(&mut h, &cid, "ap-busy-ui-compact");

    for _ in 0..2 {
        h.handle_compact_request(
            crate::harness::harness_connection_id(),
            test_session_id("s1"),
            Some(agent_id.as_str()),
        );
    }

    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentManualCompactionRequested(_)))
            .count(),
        1
    );
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );

    h.set_agent_turn_state(&cid, AgentTurnState::Idle);
    assert!(h.try_start_queued_ui_compaction(&cid));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        started.trigger,
                        tau_proto::StandaloneCompactionTrigger::ManualUi { .. }
                    )
            ))
            .count(),
        1
    );
    assert!(!h.try_start_queued_ui_compaction(&cid));
}

/// Manual compaction may claim the sole installed input wait, but it must first
/// commit one canonical cancellation that closes the provider tool round.
/// A later provider compaction failure cannot resurrect that cancelled wait.
#[test]
fn manual_compaction_cancels_sole_input_wait_before_starting() {
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
    let wait = wait_input_call("wait-preempt-compact");
    seed_assistant_tool_round(&mut h, &cid, &[(wait.id.as_str(), "wait")]);
    seed_tools_running(&mut h, &cid, vec![wait.id.clone()]);
    h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
        .expect("install input wait");

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );

    let events = event_log_events(&h);
    let cancelled = events
        .iter()
        .position(|event| {
            matches!(event, Event::ToolCancelled(cancelled) if cancelled.call_id == wait.id)
        })
        .expect("canonical wait cancellation");
    let started = events
        .iter()
        .position(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
        .expect("standalone compaction start");
    assert!(cancelled < started);
    assert!(!h.input_wait_pending_for(&cid));
    assert!(
        !h.session_runtime
            .agent_store
            .agent(&agent_id)
            .expect("agent tree")
            .has_open_foreground_tool_round()
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::AgentToolCancellationRequested(_)))
    );
    let compact_prompt = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&compact_prompt))
        .expect("compaction provider failure");
    assert!(!h.input_wait_pending_for(&cid));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| {
                matches!(event, Event::ToolCancelled(cancelled) if cancelled.call_id == wait.id)
            })
            .count(),
        1
    );
}

/// A second compact request while cancellation is intercepted must coalesce
/// without publishing another terminal or starting compaction early.
#[test]
fn repeated_manual_compaction_coalesces_while_wait_cancellation_is_parked() {
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
    let wait = wait_input_call("wait-preempt-coalesce");
    seed_assistant_tool_round(&mut h, &cid, &[(wait.id.as_str(), "wait")]);
    seed_tools_running(&mut h, &cid, vec![wait.id.clone()]);
    h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
        .expect("install input wait");
    connect_test_tool(&mut h, "wait-compact-interceptor");
    h.handle_extension_event(
        "wait-compact-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_CANCELLED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register cancellation interceptor");

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );
    h.handle_extension_event(
        "wait-compact-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release cancellation");

    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| {
                matches!(event, Event::ToolCancelled(cancelled) if cancelled.call_id == wait.id)
            })
            .count(),
        1
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        1
    );
}

/// Explicit cancellation that arrives while compact preemption is parked owns
/// teardown of the wait and suppresses the pending compaction start.
#[test]
fn explicit_cancel_while_wait_preemption_is_parked_never_compacts() {
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
    let wait = wait_input_call("wait-preempt-explicit-cancel");
    seed_assistant_tool_round(&mut h, &cid, &[(wait.id.as_str(), "wait")]);
    seed_tools_running(&mut h, &cid, vec![wait.id.clone()]);
    h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
        .expect("install input wait");
    connect_test_tool(&mut h, "wait-explicit-cancel-interceptor");
    h.handle_extension_event(
        "wait-explicit-cancel-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_CANCELLED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register cancellation interceptor");
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );

    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            agent_prompt_id: None,
        },
    );
    h.handle_extension_event(
        "wait-explicit-cancel-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release parked cancellation");

    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );
    assert!(
        !h.prompt_coordination
            .compaction_runtime
            .pending_ui_after_wait
            .contains_key(&cid)
    );
}

/// Session rollover clears a parked wait-preemption request and cannot leak a
/// late compaction start into the replacement session.
#[test]
fn session_rollover_while_wait_preemption_is_parked_never_compacts() {
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
    let wait = wait_input_call("wait-preempt-rollover");
    seed_assistant_tool_round(&mut h, &cid, &[(wait.id.as_str(), "wait")]);
    seed_tools_running(&mut h, &cid, vec![wait.id.clone()]);
    h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
        .expect("install input wait");
    connect_test_tool(&mut h, "wait-rollover-interceptor");
    h.handle_extension_event(
        "wait-rollover-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_CANCELLED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register cancellation interceptor");
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );

    h.switch_session(test_session_id("s2"), tau_proto::SessionStartReason::New)
        .expect("roll over session");

    assert!(
        h.prompt_coordination
            .compaction_runtime
            .pending_ui_after_wait
            .is_empty()
    );
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );
}

/// Cancellation append failure restores the claimed waiter without starting
/// compaction; a fresh request can retry and starts exactly one transaction.
#[test]
fn failed_wait_cancellation_append_rolls_back_without_compaction() {
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
    let wait = wait_input_call("wait-preempt-append-failure");
    seed_assistant_tool_round(&mut h, &cid, &[(wait.id.as_str(), "wait")]);
    seed_tools_running(&mut h, &cid, vec![wait.id.clone()]);
    h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
        .expect("install input wait");
    connect_test_tool(&mut h, "wait-append-failure-interceptor");
    h.handle_extension_event(
        "wait-append-failure-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_CANCELLED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register cancellation interceptor");

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "wait-append-failure-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release cancellation into append failure");

    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );
    assert_eq!(tool_result_count(&h, wait.id.as_str()), 0);
    assert!(h.input_wait_pending_for(&cid));
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    h.handle_extension_event(
        "wait-append-failure-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release fresh cancellation");
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        1
    );
    assert!(!h.input_wait_pending_for(&cid));
}

/// Provider-declared wait correlation classifies compact preemption as an
/// unknown-cause terminal plus a cancelled wait settlement, without inventing
/// a provider cancellation request.
///
/// This protects `SPEC-durable-tool-observation-correlation`.
#[test]
fn manual_compaction_preserves_cancelled_wait_correlation() {
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
    seed_assistant_tool_round(&mut h, &cid, &[("wait-preempt-correlation", "wait")]);
    let declaration = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent journal")
        .into_iter()
        .find_map(|record| {
            matches!(record.event, Event::ProviderResponseFinished(_))
                .then_some(record.observation_id)
        })
        .expect("assistant declaration");
    let wait = AgentToolCall {
        call_ref: Some(tau_proto::ToolCallRef {
            declaration,
            item_index: 0,
        }),
        id: "wait-preempt-correlation".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: wait_input_call("unused").arguments,
    };
    seed_tools_running(&mut h, &cid, vec![wait.id.clone()]);
    h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
        .expect("install provider-declared wait");

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );

    let events: Vec<_> = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent journal")
        .into_iter()
        .map(|record| record.event)
        .collect();
    let terminal = events
        .iter()
        .position(|event| matches!(event, Event::ToolCancelled(_)))
        .expect("canonical terminal");
    let settled = events
        .iter()
        .position(|event| {
            matches!(event, Event::AgentToolWaitSettled(settled)
                if settled.outcome == tau_proto::ToolWaitOutcome::Cancelled)
        })
        .expect("cancelled wait settlement");
    assert!(terminal < settled);
    assert!(events.iter().any(|event| {
        matches!(event, Event::AgentToolTerminalClassified(classified)
            if classified.cause == tau_proto::ToolTerminalCause::Unknown)
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::AgentToolCancellationRequested(_)))
    );
}

/// A terminal publication already owns the wait race, so a later compact
/// request must retain the ordinary busy rejection without claiming the waiter.
#[test]
fn manual_compaction_rejects_already_terminalizing_wait() {
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
    let wait = wait_input_call("wait-terminalizing-before-compact");
    seed_assistant_tool_round(&mut h, &cid, &[(wait.id.as_str(), "wait")]);
    seed_tools_running(&mut h, &cid, vec![wait.id.clone()]);
    h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
        .expect("install input wait");
    h.tool_routing
        .tool_runtime
        .pending_terminal_observations
        .insert(
            wait.id.clone(),
            crate::harness::PendingTerminalObservation {
                observation_id: tau_proto::ObservationId::random(),
                cause: tau_proto::ToolTerminalCause::LifecycleTeardown,
            },
        );

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );

    assert!(h.input_wait_pending_for(&cid));
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(event, Event::ToolCancelled(cancelled) if cancelled.call_id == wait.id)
            || matches!(event, Event::AgentStandaloneCompactionStarted(_))
    }));
}

/// Legacy inline compaction applies the same cancel-then-compact boundary as
/// standalone provider compaction.
#[test]
fn manual_inline_compaction_cancels_sole_input_wait_first() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let wait = wait_input_call("wait-preempt-inline-compact");
    seed_assistant_tool_round(&mut h, &cid, &[(wait.id.as_str(), "wait")]);
    seed_tools_running(&mut h, &cid, vec![wait.id.clone()]);
    h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
        .expect("install input wait");

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );

    let events = event_log_events(&h);
    let cancelled = events
        .iter()
        .position(|event| {
            matches!(event, Event::ToolCancelled(cancelled) if cancelled.call_id == wait.id)
        })
        .expect("canonical wait cancellation");
    let triggered = events
        .iter()
        .position(|event| matches!(event, Event::AgentCompactionTriggered(_)))
        .expect("inline compaction trigger");
    assert!(cancelled < triggered);
    assert!(!h.input_wait_pending_for(&cid));
}

/// A non-cancelled standalone-compaction failure is a warm terminal path even
/// though it has no final inference response. An explicit-parent typed start
/// has no tool-call discriminator, so warm completion and cold restore must
/// both use its durable ancestry and keep the detached worker addressable.
#[test]
fn explicit_parent_compaction_failed_worker_remains_loaded_across_resume() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let worker_agent_id = {
        let mut h = quiet_provider_harness(&sp).expect("start");
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .supports_standalone_compaction = true;
        let parent_cid = ensure_test_user_agent(&mut h);
        let parent_agent_id = durable_agent_id_for_conversation(&h, &parent_cid);
        let mut query = ext_query("q-compaction-failure");
        query.parent_agent = Some(parent_agent_id);
        h.handle_start_agent_request(&crate::test_connection_id(HARNESS_CONNECTION_ID), query)
            .expect("start worker");
        let worker_cid = ext_query_cid(&h, "q-compaction-failure").expect("worker");
        let worker_agent_id = durable_agent_id_for_conversation(&h, &worker_cid);
        let inference = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(context_overflow_response(&inference))
            .expect("start reactive recovery");
        let compact = read_nth_prompt_created(&h, 1);
        h.handle_provider_response_finished(context_overflow_response(&compact))
            .expect("fail reactive compaction");
        assert!(
            h.agent_runtime.agent_registry.agents[&worker_cid]
                .identity
                .originator
                .is_user()
        );
        assert!(
            h.agent_runtime.agent_registry.agents[&worker_cid]
                .identity
                .parent_agent_id
                .is_none()
        );
        assert!(
            h.agent_runtime.agent_registry.agents[&worker_cid]
                .identity
                .parent_tool_call_id
                .is_none()
        );
        assert!(
            h.agent_runtime
                .agent_registry
                .session_loaded
                .contains(&worker_agent_id)
        );
        assert_eq!(
            event_log_count(&h, |event| matches!(
                event,
                Event::StartAgentResult(result) if result.query_id == "q-compaction-failure"
            )),
            1
        );
        assert!(!event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::SessionAgentUnloaded(unloaded) if unloaded.agent_id == worker_agent_id
        )));
        h.shutdown().expect("shutdown completed failure");
        worker_agent_id
    };

    let mut resumed =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume completed failure");
    resumed.config.selected_model = Some("test/model".into());
    let worker_cid = resumed
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(worker_agent_id.as_str())
        .cloned()
        .expect("restored worker route");
    assert!(
        resumed.agent_runtime.agent_registry.agents[&worker_cid]
            .identity
            .originator
            .is_user()
    );
    assert!(matches!(
        resumed.agent_runtime.agent_registry.agents[&worker_cid]
            .turn
            .turn_state,
        AgentTurnState::Idle
    ));
    assert_eq!(
        resumed
            .agent_runtime
            .agent_registry
            .navigation_modes
            .get(&worker_agent_id),
        Some(&tau_proto::AgentNavigationMode::ActiveAuto)
    );
    resumed
        .handle_authenticated_ui_prompt_submitted(
            crate::harness::harness_connection_id(),
            UiPromptSubmitted {
                literal: false,
                session_id: test_session_id("s1"),
                text: "fresh after compaction failure".to_owned(),
                agent_id: worker_agent_id.clone(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            },
        )
        .expect("submit fresh worker turn");
    assert!(
        resumed.agent_runtime.agent_registry.agents[&worker_cid]
            .dispatch
            .pending_prompts
            .iter()
            .any(|prompt| prompt.text == "fresh after compaction failure")
            || event_log_events(&resumed).iter().any(|event| matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::Inference
            )),
        "a fresh turn must either queue or dispatch after the terminal failure"
    );
    assert_eq!(
        resumed
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(worker_agent_id.as_str()),
        Some(&worker_cid)
    );
    assert!(!event_log_contains_any_source(&resumed, |event| matches!(
        event,
        Event::StartAgentResult(result) if result.query_id == "q-compaction-failure"
    )));
    assert!(!event_log_contains_any_source(&resumed, |event| matches!(
        event,
        Event::SessionAgentUnloaded(unloaded) if unloaded.agent_id == worker_agent_id
    )));
    resumed.shutdown().expect("shutdown resumed worker");
}

#[test]
fn manual_compact_appends_trigger_and_dispatches_normal_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    enable_remote_compaction_for_test_model(&mut h);

    let cid = ensure_test_user_agent(&mut h);
    let target_agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent id");
    let selected_role = h.config.selected_role.clone();
    h.config
        .available_roles
        .get_mut(&selected_role)
        .expect("selected role")
        .compaction = Some(path_tau_config_settings::RoleCompaction::Threshold(1200));

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&target_agent_id),
    );

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentCompactionTriggered(triggered)
            if triggered.agent_id.as_str() == target_agent_id.as_str()
    )));
    let mut cursor = path_crate_event_log::EventLogSeq::new(0);
    let mut prompt = None;
    while let Some(entry) = h.runtime_io.event_log.get_next_from(cursor) {
        cursor = entry.seq.next();
        if let Event::AgentPromptCreated(created) = entry.event {
            prompt = Some(created);
        }
    }
    let prompt = prompt.expect("normal prompt created");
    assert!(
        prompt
            .context
            .flatten()
            .contains(&ContextItem::CompactionTrigger)
    );
    assert_eq!(
        prompt.compaction,
        Some(tau_proto::PromptCompactionContext {
            compact_threshold: Some(tau_proto::TokenCount::new(1200)),
        })
    );

    h.shutdown().expect("shutdown");
}

/// An ordinary response that installs an inline compaction boundary must not
/// issue a stale advisory and must re-arm alert crossings with context usage.
#[test]
fn inline_compaction_response_resets_context_size_alerts_without_injection() {
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
                message: "stale compact advice".to_owned(),
                when: tau_config::settings::ContextPolicyWhen {
                    at: path_tau_config_settings::ContextPolicyPoint::AfterResponse,
                    statuses: None,
                },
            },
        );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .fired_context_size_alerts
        .insert("compact-soon".to_owned());
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("work".to_owned()))
        .expect("dispatch");
    let prompt = read_nth_prompt_created(&h, 0);
    let mut response = provider_text_response(&prompt.agent_prompt_id, prompt.agent_id, "ignored");
    response.output_items = vec![ContextItem::Compaction(
        tau_proto::OpaqueProviderItem::from_raw_json(r#"{"type":"compaction"}"#)
            .expect("valid compaction item"),
    )];
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 101,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 1,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(response)
        .expect("finish response");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == "stale compact advice"
    )));
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .fired_context_size_alerts
            .is_empty()
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        None
    );
    h.shutdown().expect("shutdown");
}

/// A compaction reached while one of several crossed alerts is running must
/// discard the remaining queued alerts from the obsolete usage climb.
#[test]
fn inline_compaction_discards_other_queued_context_size_alerts() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    for (name, message) in [
        ("alert-a", "first alert"),
        ("alert-b", "stale second alert"),
    ] {
        role.context_size_alerts.insert(
            name.to_owned(),
            tau_config::settings::ContextSizeAlert {
                threshold: path_tau_config_settings::ContextSizeAlertThreshold::new(100)
                    .expect("positive test threshold"),
                enable: true,
                message: message.to_owned(),
                when: tau_config::settings::ContextPolicyWhen {
                    at: path_tau_config_settings::ContextPolicyPoint::AfterResponse,
                    statuses: None,
                },
            },
        );
    }

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("work".to_owned()))
        .expect("dispatch work");
    let work = read_nth_prompt_created(&h, 0);
    let mut work_response =
        provider_text_response(&work.agent_prompt_id, work.agent_id, "finished work");
    work_response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 101,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 1,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(work_response)
        .expect("finish work");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == "first alert"
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == "stale second alert"
    )));
    let first_alert = read_nth_prompt_created(&h, 1);
    let mut compacting_response = provider_text_response(
        &first_alert.agent_prompt_id,
        first_alert.agent_id,
        "ignored",
    );
    compacting_response.output_items = vec![ContextItem::Compaction(
        tau_proto::OpaqueProviderItem::from_raw_json(r#"{"type":"compaction"}"#)
            .expect("valid compaction item"),
    )];
    h.handle_provider_response_finished(compacting_response)
        .expect("finish alert with compaction");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == "stale second alert"
    )));
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| !prompt.is_context_size_alert())
    );
    h.shutdown().expect("shutdown");
}
/// Cold replay must restore the newest exact provider observation, including
/// its durable prompt id, before scheduling the first resumed activation.
#[test]
fn cold_resume_restores_exact_usage_for_first_activation_compaction() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    seed_agent_context_usage(&state, Some("test/model"), 900);

    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(900));
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id,
            text: "historical prefix ".repeat(80),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let agent = &h.agent_runtime.agent_registry.agents[&cid];
    assert_eq!(agent.execution.context_input_tokens, Some(900));
    assert!(agent.execution.context_usage_prompt_id.is_some());
    assert!(agent.execution.context_usage_head.is_some());

    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::user("first resumed activation".to_owned()),
    )
    .expect("dispatch resumed activation");
    assert_eq!(
        read_nth_prompt_created(&h, 0).operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    h.shutdown().expect("shutdown");
}

/// Any accepted compaction boundary invalidates prior usage both live and on
/// the next cold replay because the replacement has a new token baseline.
#[test]
fn agent_compacted_resets_live_and_restored_context_usage() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    seed_agent_context_usage(&state, Some("test/model"), 900);
    {
        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let cid = ensure_test_user_agent(&mut h);
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_input_tokens,
            Some(900)
        );
        h.publish_for_agent(
            &cid,
            Event::AgentCompacted(tau_proto::AgentCompacted {
                original_input_tokens: None,
                compaction_output_tokens: None,
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                transaction_id: None,
                cut: None,
                suffix_end: None,
                compact_prompt_id: None,
                model: None,
                operation: None,
                replacement_window: vec![ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content: vec![ContentPart::Text {
                        text: "replacement".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                })],
            }),
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_input_tokens,
            None
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_usage_head,
            None
        );
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");

    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume after compact");
    let cid = ensure_test_user_agent(&mut h);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        None
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_cached_tokens,
        None
    );
    h.shutdown().expect("shutdown");
}

/// A standalone-capable model must dispatch an explicit compact operation and
/// accept its validated output as exactly one replacement-window boundary.
#[test]
fn manual_standalone_compact_installs_one_boundary() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(900));
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(
        prompt.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    let events = event_log_events(&h);
    let (started_index, started) = events
        .iter()
        .enumerate()
        .find_map(|(index, event)| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some((index, started.clone())),
            _ => None,
        })
        .expect("durable start");
    let prompt_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentPromptCreated(created)
                    if created.agent_prompt_id == prompt.agent_prompt_id
            )
        })
        .expect("provider prompt");
    assert!(
        started_index < prompt_index,
        "durable start must commit before provider dispatch"
    );
    assert_eq!(started.compact_prompt_id, prompt.agent_prompt_id);
    assert_eq!(started.model, prompt.model);
    assert_eq!(started.operation, prompt.operation);
    let response = ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: prompt.agent_prompt_id,
        agent_id: crate::parse_agent_id(&agent_id),
        output_items: vec![ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::Assistant,
            content: vec![tau_proto::ContentPart::Text {
                text: "summary".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens: 226_200,
            response_received_tokens: 4_500,
            ..Default::default()
        }),
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    };
    h.handle_provider_response_finished(response.clone())
        .expect("accept compact response");
    h.handle_provider_response_finished(response)
        .expect("ignore duplicate compact response");

    let compacted: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentCompacted(compacted) => Some(compacted),
            _ => None,
        })
        .collect();
    assert_eq!(compacted.len(), 1);
    assert_eq!(
        (
            compacted[0].original_input_tokens,
            compacted[0].compaction_output_tokens,
        ),
        (
            Some(tau_proto::TokenCount::new(226_200)),
            Some(tau_proto::TokenCount::new(4_500)),
        ),
        "the durable boundary must retain canonical provider usage"
    );
    assert_eq!(
        (
            compacted[0].compact_prompt_id.as_ref(),
            compacted[0].model.as_ref(),
            compacted[0].operation,
        ),
        (
            Some(&started.compact_prompt_id),
            Some(&started.model),
            Some(started.operation),
        ),
        "terminal correlation must be copied from the durable start"
    );
    assert!(matches!(
        agent_tree_for_conversation(&h, &cid).current_branch().last(),
        Some(tau_core::AgentEntry::Compaction {
            replacement_window, ..
        })
            if replacement_window.len() == 1
    ));
    h.shutdown().expect("shutdown");
}

/// A canonical no-output inference rejection commits the harness-authored plan
/// before one durable claim, dispatches one compact request, and continues the
/// owed activation exactly once after the accepted replacement boundary.
///
/// This test cluster covers durable ordering, fail-closed eligibility, replay,
/// crash cuts, and continuation under
/// `SPEC-compaction-and-context-recovery`.
#[test]
fn reactive_context_overflow_recovers_in_durable_order_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    seed_reactive_compaction_prefix(&mut h, &cid);
    let model: tau_proto::ModelId = "test/model".into();
    let provider_owner = h.provider_runtime.model_routes[&model].clone();
    let serving_model = h
        .provider_runtime
        .models_by_extension
        .get_mut(provider_owner.as_str())
        .expect("provider-owned model snapshot")
        .iter_mut()
        .rfind(|candidate| candidate.id == model)
        .expect("serving model");
    serving_model.est_uncached_input_cost_1m_usd =
        tau_proto::EstimatedUsdPerMillion::checked_from_usd(1);
    serving_model.est_cached_input_cost_1m_usd =
        tau_proto::EstimatedUsdPerMillion::checked_from_usd(1);
    serving_model.est_output_cost_1m_usd = tau_proto::EstimatedUsdPerMillion::checked_from_usd(1);

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow activation".to_owned()))
        .expect("dispatch inference");
    let inference = read_nth_prompt_created(&h, 0);
    let serving_model = h
        .provider_runtime
        .models_by_extension
        .get_mut(provider_owner.as_str())
        .expect("provider-owned model snapshot")
        .iter_mut()
        .rfind(|candidate| candidate.id == model)
        .expect("serving model");
    serving_model.est_uncached_input_cost_1m_usd =
        tau_proto::EstimatedUsdPerMillion::checked_from_usd(2);
    serving_model.est_cached_input_cost_1m_usd =
        tau_proto::EstimatedUsdPerMillion::checked_from_usd(2);
    serving_model.est_output_cost_1m_usd = tau_proto::EstimatedUsdPerMillion::checked_from_usd(2);
    let turn_generation = h.agent_runtime.agent_registry.agents[&cid]
        .turn
        .turn_generation;
    let mut rejected = context_overflow_response(&inference);
    rejected.usage = Some(tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000_000,
        ..tau_proto::ProviderTokenUsage::default()
    });
    rejected.context_limit_telemetry = Some(tau_proto::ContextLimitTelemetry {
        model: "evil/forged".parse().expect("model"),
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        transcript_delta_bytes: Some(tau_proto::ByteCount::new(1)),
        advertised_context_window: Some(tau_proto::TokenCount::new(1)),
        provider_input_tokens: Some(tau_proto::TokenCount::new(1)),
        compaction_threshold: Some(tau_proto::TokenCount::new(1)),
        compaction_policy: tau_proto::ContextLimitCompactionPolicy::Disabled,
        recovery_eligible: false,
        action: tau_proto::ContextLimitAction::Terminal,
        observation: tau_proto::ContextLimitObservation::RejectedAtOrAboveAdvertisedLimit,
    });
    h.handle_provider_response_finished(rejected.clone())
        .expect("plan reactive recovery");
    assert_eq!(
        h.agent_stats_snapshot(&cid)
            .expect("post-rejection stats")
            .estimated_api_cost
            .as_picodollars(),
        1_000_000_000_000,
        "accepted overflow usage uses the inference dispatch snapshot"
    );
    h.handle_provider_response_finished(rejected)
        .expect("ignore duplicate overflow terminal");
    assert_eq!(
        h.agent_stats_snapshot(&cid)
            .expect("post-duplicate stats")
            .estimated_api_cost
            .as_picodollars(),
        1_000_000_000_000,
        "duplicate overflow terminal must not charge twice"
    );
    let compact = read_nth_prompt_created(&h, 1);
    assert_eq!(
        compact.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );

    let events = event_log_events(&h);
    let planned_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == inference.agent_prompt_id
                        && response.recovery_disposition
                            == tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned
            )
        })
        .expect("planned terminal response");
    let planned = match &events[planned_index] {
        Event::ProviderResponseFinished(response) => response,
        _ => unreachable!("located provider response"),
    };
    let telemetry = planned
        .context_limit_telemetry
        .as_ref()
        .expect("harness attaches dispatch snapshot");
    assert_eq!(telemetry.model, "test/model".parse().expect("model"));
    assert_eq!(telemetry.operation, tau_proto::PromptOperation::Inference);
    assert_eq!(
        telemetry.advertised_context_window,
        Some(tau_proto::TokenCount::new(1000))
    );
    assert!(
        telemetry
            .transcript_delta_bytes
            .is_some_and(|bytes| bytes > tau_proto::ByteCount::ZERO)
    );
    assert_eq!(
        telemetry.provider_input_tokens,
        Some(tau_proto::TokenCount::new(1_000_000))
    );
    assert_eq!(
        telemetry.compaction_policy,
        tau_proto::ContextLimitCompactionPolicy::ProviderDefault
    );
    assert_eq!(
        telemetry.observation,
        tau_proto::ContextLimitObservation::RejectedAtOrAboveAdvertisedLimit
    );
    assert!(telemetry.recovery_eligible);
    assert_eq!(
        telemetry.action,
        tau_proto::ContextLimitAction::ReactiveCompactionPlanned
    );
    assert_eq!(
        planned.recovery_disposition,
        tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .context_limits
            .contains_key(&inference.agent_prompt_id),
        "terminal response consumes prompt-local telemetry snapshot"
    );
    let (start_index, start) = events
        .iter()
        .enumerate()
        .find_map(|(index, event)| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some((index, started)),
            _ => None,
        })
        .expect("reactive start");
    assert!(planned_index < start_index);
    assert!(matches!(
        &start.trigger,
        tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
            failed_agent_prompt_id
        } if failed_agent_prompt_id == &inference.agent_prompt_id
    ));
    assert_eq!(start.compact_prompt_id, compact.agent_prompt_id);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .turn_generation,
        turn_generation
    );
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    assert!(matches!(
        h.agent_runtime.agent_watch.provider_status[agent_id.as_str()].state,
        tau_proto::AgentWatchProviderState::RecoveringContext { .. }
    ));

    let mut compact_response = strict_fake_compact_response(&compact)
        .expect("strict fake provider accepts only a balanced compact timeline");
    compact_response.usage = Some(tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000_000,
        ..tau_proto::ProviderTokenUsage::default()
    });
    h.handle_provider_response_finished(compact_response)
        .expect("accept compact response");
    assert_eq!(
        h.agent_stats_snapshot(&cid)
            .expect("post-compaction stats")
            .estimated_api_cost
            .as_picodollars(),
        3_000_000_000_000,
        "compaction usage uses its fresh $2 dispatch snapshot"
    );
    let continuation = read_nth_prompt_created(&h, 2);
    assert_eq!(
        continuation.operation,
        tau_proto::PromptOperation::Inference
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .turn_generation,
        turn_generation,
        "recovery and continuation remain one logical watched turn"
    );
    assert!(
        !event_log_events(&h)[planned_index..]
            .iter()
            .any(|event| matches!(
                event,
                Event::AgentState(changed)
                    if changed.agent_id.as_str() == agent_id
                        && changed.state == tau_proto::AgentRuntimeState::Idle
            )),
        "no turn-stop may be emitted between rejection and continuation"
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        started.trigger,
                        tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
                    )
            ))
            .count(),
        1
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentPromptStarted(prompt)
                    if prompt.operation == tau_proto::PromptOperation::Inference
            ))
            .count(),
        2,
        "original plus exactly one continuation"
    );

    h.handle_provider_response_finished(context_overflow_response(&continuation))
        .expect("post-compaction overflow is terminal");
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        1,
        "post-compaction inference cannot recursively compact"
    );
    h.shutdown().expect("shutdown");
}

/// Provider-authored disposition, semantic streamed output, final output,
/// disabled policy, unsupported capability, and model mismatch must all fail
/// closed without publishing a reactive transaction.
#[test]
fn reactive_context_overflow_eligibility_fails_closed() {
    #[derive(Clone, Copy)]
    enum Case {
        ForgedDisposition,
        StreamedOutput,
        FinalOutput,
        ToolOutput,
        Disabled,
        Unsupported,
        ModelMismatch,
        CurrentModelMismatch,
        BranchMismatch,
    }
    for case in [
        Case::ForgedDisposition,
        Case::StreamedOutput,
        Case::FinalOutput,
        Case::ToolOutput,
        Case::Disabled,
        Case::Unsupported,
        Case::ModelMismatch,
        Case::CurrentModelMismatch,
        Case::BranchMismatch,
    ] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .supports_standalone_compaction =
            !matches!(case, Case::Unsupported | Case::ForgedDisposition);
        if matches!(case, Case::Disabled) {
            let role = h.config.selected_role.clone();
            h.config.available_roles.entry(role).or_default().compaction =
                Some(path_tau_config_settings::RoleCompaction::Disabled);
        }
        let cid = ensure_test_user_agent(&mut h);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow".to_owned()))
            .expect("dispatch inference");
        let prompt = read_nth_prompt_created(&h, 0);
        if matches!(case, Case::StreamedOutput) {
            let provider = h
                .provider_runtime
                .pending_prompts
                .get(&prompt.agent_prompt_id)
                .cloned()
                .expect("provider owner");
            h.handle_extension_event(
                provider.as_str(),
                TestProtocolItem::Event(Event::ProviderResponseUpdatedReported(
                    ProviderResponseUpdated {
                        agent_prompt_id: prompt.agent_prompt_id.clone(),
                        agent_id: prompt.agent_id.clone(),
                        deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
                            output_index: 0,
                            text: "accepted partial output".to_owned(),
                            phase: None,
                        }],
                        compaction: None,
                        status: None,
                        response_stats: None,
                        originator: prompt.originator.clone(),
                    },
                )),
            )
            .expect("ingest semantic stream delta");
        }
        if matches!(case, Case::ModelMismatch) {
            h.prompt_coordination
                .prompt_runtime
                .models
                .insert(prompt.agent_prompt_id.clone(), "provider/other".into());
        }
        if matches!(case, Case::CurrentModelMismatch) {
            let mut other = h
                .provider_runtime
                .model_info
                .get(&"test/model".into())
                .expect("test model")
                .clone();
            other.id = "provider/other".into();
            h.provider_runtime
                .model_info
                .insert(other.id.clone(), other);
            h.config.selected_model = Some("provider/other".into());
        }
        if matches!(case, Case::BranchMismatch) {
            h.publish_for_agent(
                &cid,
                Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                    agent_id: prompt.agent_id.clone(),
                    head: tau_proto::AgentHead::Root,
                }),
            );
        }
        let mut response = context_overflow_response(&prompt);
        if matches!(case, Case::FinalOutput) {
            response
                .output_items
                .push(ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content: vec![ContentPart::Text {
                        text: "partial".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                }));
        }
        if matches!(case, Case::ToolOutput) {
            response
                .output_items
                .push(ContextItem::ToolCall(ToolCallItem {
                    call_id: "call-overflow".into(),
                    name: ToolName::new("ignored"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Map(Vec::new()),
                    raw_arguments_json: None,
                    responses_envelope: None,
                }));
            response.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
        }
        if matches!(case, Case::ForgedDisposition) {
            response.recovery_disposition =
                tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned;
        }
        h.handle_provider_response_finished(response)
            .expect("terminal handling");
        assert!(
            !event_log_events(&h)
                .iter()
                .any(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_))),
            "ineligible case must not compact"
        );
        if matches!(case, Case::ForgedDisposition) {
            assert!(event_log_events(&h).iter().any(|event| matches!(
                event,
                Event::ProviderResponseFinished(finished)
                    if finished.agent_prompt_id == prompt.agent_prompt_id
                        && finished.recovery_disposition
                            == tau_proto::ContextRecoveryDisposition::None
            )));
        }
        h.shutdown().expect("shutdown");
    }
}

/// Replay after the planned terminal response but before its claim must restore
/// the runtime route first, publish one claim, and dispatch one compact prompt.
#[test]
fn reactive_context_overflow_replay_claims_and_dispatches_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("start");
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    h.config.selected_model = Some("provider-b/model".into());
    let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
    h.publish_event(
        None,
        Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: test_session_id("s1"),
            agent_id: agent_id.clone(),
            ephemeral: false,
        }),
    );
    let mut store = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
    append_seed_agent_event(
        &mut store,
        Event::AgentStarted(tau_proto::AgentStarted {
            creator: Some(tau_proto::AgentCreator::default()),

            parent_agent: None,
            agent_id: agent_id.clone(),
            role: "engineer".to_owned(),
            display_name: None,
            metadata: Vec::new(),
            ephemeral: false,
        }),
    );
    append_seed_agent_event(
        &mut store,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: agent_id.clone(),
            inference_activation: true,
            text: "owed activation".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let through = tau_proto::AgentHead::Node(
        store
            .agent("main")
            .and_then(tau_core::AgentTree::head)
            .expect("head"),
    );
    let prompt_id = test_agent_prompt_id("ap-main-overflow");
    append_seed_agent_event(
        &mut store,
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            output_length_continuation: None,
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: prompt_id.clone(),
            through,
            model: Some("provider-b/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
        }),
    );
    let planned = ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: prompt_id,
        agent_id,
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("bounded".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    };
    append_seed_agent_event(&mut store, Event::ProviderResponseFinished(planned));
    drop(store);

    let mut capable = h
        .provider_runtime
        .model_info
        .get(&"test/model".into())
        .expect("test model")
        .clone();
    capable.id = "provider-b/model".into();
    let provider_owner = h.provider_runtime.model_routes[&"test/model".into()].to_string();
    h.rehydrate_agents_from_session();
    let restored_cid = h.agent_runtime.agent_registry.agent_routes["main"].clone();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&restored_cid)
        .expect("restored agent")
        .identity
        .model_override = Some("provider-b/model".into());
    let mut unrelated = capable.clone();
    unrelated.id = "provider-a/other".into();
    h.publish_provider_models_update(
        &crate::test_connection_id("provider-a"),
        tau_proto::ExtensionName::parse("provider-a")
            .expect("test extension name must satisfy the identifier grammar"),
        tau_proto::ProviderModelsDeclared {
            models: vec![unrelated],
        },
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&restored_cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::ContextRecoveryPending { .. }
    ));
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );
    h.publish_provider_models_update(
        &crate::test_connection_id(&provider_owner),
        crate::test_extension_name(provider_owner.clone()),
        tau_proto::ProviderModelsDeclared {
            models: vec![capable],
        },
    );
    h.drain_publish_idle_dispatches();
    assert!(
        matches!(
            h.agent_runtime.agent_registry.agents[&restored_cid]
                .dispatch
                .activation_dispatch,
            crate::agent::ActivationDispatchState::Running { .. }
        ),
        "{:?}",
        h.agent_runtime.agent_registry.agents[&restored_cid]
            .dispatch
            .activation_dispatch
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        started.trigger,
                        tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
                    )
            ))
            .count(),
        1
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
            ))
            .count(),
        1
    );
    h.shutdown().expect("shutdown");
}

/// Authoritative replay drift terminalizes a real correlated transaction,
/// exposes blocked status, and leaves explicit manual compaction usable.
#[test]
fn reactive_context_overflow_replay_drift_allows_manual_compact() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    seed_main_agent_loaded(&state);
    let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
    let mut store = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
    append_seed_agent_event(
        &mut store,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: agent_id.clone(),
            inference_activation: true,
            text: "owed".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let through = tau_proto::AgentHead::Node(
        store
            .agent("main")
            .and_then(tau_core::AgentTree::head)
            .expect("head"),
    );
    let prompt_id = test_agent_prompt_id("ap-main-drift");
    append_seed_agent_event(
        &mut store,
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            output_length_continuation: None,
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: prompt_id.clone(),
            through,
            model: Some("provider-b/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
        }),
    );
    append_seed_agent_event(
        &mut store,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: prompt_id,
            agent_id: agent_id.clone(),
            output_items: Vec::new(),
            stop_reason: tau_proto::ProviderStopReason::Error,
            error: Some("bounded".to_owned()),
            failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned,
            originator: tau_proto::PromptOriginator::User,
            usage: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    drop(store);

    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let unsupported = h
        .provider_runtime
        .model_info
        .get(&"test/model".into())
        .expect("test model")
        .clone();
    h.publish_provider_models_update(
        &crate::test_connection_id("provider-recovery-owner"),
        tau_proto::ExtensionName::parse("provider-recovery-owner")
            .expect("test extension name must satisfy the identifier grammar"),
        tau_proto::ProviderModelsDeclared {
            models: vec![unsupported],
        },
    );
    assert!(
        h.agent_runtime
            .agent_watch
            .provider_status
            .get("main")
            .is_none_or(|status| !matches!(
                status.state,
                tau_proto::AgentWatchProviderState::Blocked { .. }
            )),
        "terminal recovery drift must not leave the agent blocked"
    );
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some("main"),
    );
    assert_eq!(
        read_nth_prompt_created(&h, 0).operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    h.shutdown().expect("shutdown");
}

/// UI cancellation during reactive compaction publishes one durable Cancelled
/// outcome; a late provider terminal and cold replay cannot duplicate it.
#[test]
fn reactive_context_overflow_ui_cancel_is_terminal_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (agent_id, compact);
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .supports_standalone_compaction = true;
        let cid = ensure_test_user_agent(&mut h);
        seed_reactive_compaction_prefix(&mut h, &cid);
        agent_id = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .clone()
            .expect("agent id");
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow".to_owned()))
            .expect("dispatch inference");
        let inference = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(context_overflow_response(&inference))
            .expect("start recovery");
        compact = read_nth_prompt_created(&h, 1);
        h.handle_cancel_prompt(
            crate::harness::harness_connection_id(),
            &tau_proto::UiCancelPrompt {
                session_id: test_session_id("s1"),
                target_agent_id: Some(crate::parse_agent_id(&agent_id)),
                agent_prompt_id: Some(compact.agent_prompt_id.clone()),
            },
        );
        h.handle_provider_response_finished(context_overflow_response(&compact))
            .expect("discard late response");
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::AgentStandaloneCompactionFailed(failed)
                        if failed.reason
                            == tau_proto::StandaloneCompactionFailureReason::Cancelled
                ))
                .count(),
            1
        );
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert!(
        !event_log_events(&resumed).iter().any(|event| matches!(
            event,
            Event::AgentPromptStarted(prompt)
                if prompt.agent_prompt_id == compact.agent_prompt_id
        )),
        "terminal cancelled compaction is not replay-dispatched"
    );
    assert!(
        resumed
            .agent_runtime
            .agent_watch
            .provider_status
            .get(&agent_id)
            .is_none_or(|status| !matches!(
                status.state,
                tau_proto::AgentWatchProviderState::Blocked { .. }
            )),
        "a durable cancelled terminal must replay as usable"
    );
    resumed.shutdown().expect("shutdown");
}

/// A delegated side conversation whose reactive compact fails must return one
/// safe terminal result and detach instead of leaving `agent_start` pending.
#[test]
fn reactive_context_overflow_side_failure_completes_request() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    h.handle_start_agent_request(
        &crate::test_connection_id(HARNESS_CONNECTION_ID),
        ext_query("q-reactive-failure"),
    )
    .expect("start side agent");
    let side_cid = ext_query_cid(&h, "q-reactive-failure").expect("side agent");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("start recovery");
    let compact = read_nth_prompt_created(&h, 1);
    h.handle_provider_response_finished(context_overflow_response(&compact))
        .expect("fail compact");

    assert!(
        !h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&side_cid)
    );
    let results: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::StartAgentResult(result) if result.query_id == "q-reactive-failure" => {
                Some(result)
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].error.as_deref(),
        Some("provider failure: compaction")
    );
    assert!(
        !serde_json::to_string(&results[0])
            .expect("serialize result")
            .contains("bounded context rejection")
    );
    h.shutdown().expect("shutdown");
}

/// A crash with the reactive compact claim committed but no terminal result
/// records Interrupted on replay and never redispatches the ambiguous request.
#[test]
fn reactive_context_overflow_claimed_crash_is_not_redispatched() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let compact_prompt_id;
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .supports_standalone_compaction = true;
        let cid = ensure_test_user_agent(&mut h);
        seed_reactive_compaction_prefix(&mut h, &cid);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow".to_owned()))
            .expect("dispatch inference");
        let inference = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(context_overflow_response(&inference))
            .expect("claim recovery");
        compact_prompt_id = read_nth_prompt_created(&h, 1).agent_prompt_id;
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert!(
        !event_log_events(&resumed).iter().any(|event| matches!(
            event,
            Event::AgentPromptStarted(prompt)
                if prompt.agent_prompt_id == compact_prompt_id
        )),
        "ambiguous compact request is never replayed"
    );
    let tree = resumed.session_runtime.agent_store.agent(
        resumed
            .agent_runtime
            .agent_registry
            .agent_routes
            .keys()
            .next()
            .expect("restored durable agent"),
    );
    assert!(matches!(
        tree.and_then(tau_core::AgentTree::standalone_compaction_recovery),
        Some(tau_core::StandaloneCompactionRecovery::Blocked { failed, .. })
            if failed.reason == tau_proto::StandaloneCompactionFailureReason::Interrupted
    ));
    resumed.shutdown().expect("shutdown");
}

/// Discovery-complete absence after compact success commits one exact
/// continuation checkpoint, terminalizes it without remote inference, and does
/// not resend if the captured model later appears or the session replays.
#[test]
fn reactive_context_overflow_compact_success_resumes_one_checkpoint() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    seed_main_agent_loaded(&state);
    let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
    let mut store = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
    append_seed_agent_event(
        &mut store,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: agent_id.clone(),
            inference_activation: true,
            text: "owed".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let through = tau_proto::AgentHead::Node(
        store
            .agent("main")
            .and_then(tau_core::AgentTree::head)
            .expect("head"),
    );
    let failed_prompt_id = test_agent_prompt_id("ap-main-overflow-success-cut");
    append_seed_agent_event(
        &mut store,
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            output_length_continuation: None,
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: failed_prompt_id.clone(),
            through,
            model: Some("provider-b/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
        }),
    );
    append_seed_agent_event(
        &mut store,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: failed_prompt_id.clone(),
            agent_id: agent_id.clone(),
            output_items: Vec::new(),
            stop_reason: tau_proto::ProviderStopReason::Error,
            error: Some("bounded".to_owned()),
            failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned,
            originator: tau_proto::PromptOriginator::User,
            usage: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-reactive-success-cut").expect("id");
    let compact_prompt_id = test_agent_prompt_id("ap-main-compact-success-cut");
    append_seed_agent_event(
        &mut store,
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            agent_id: agent_id.clone(),
            transaction_id: transaction_id.clone(),
            compact_prompt_id: compact_prompt_id.clone(),
            cut: tau_proto::AgentHead::Root,
            resume_through: Some(through),
            model: "provider-b/model".into(),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: tau_proto::PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                failed_agent_prompt_id: failed_prompt_id,
            },
        }),
    );
    let suffix_end = tau_proto::AgentHead::Node(
        store
            .agent("main")
            .and_then(tau_core::AgentTree::head)
            .expect("suffix head"),
    );
    append_seed_agent_event(
        &mut store,
        Event::AgentCompacted(tau_proto::AgentCompacted {
            original_input_tokens: None,
            compaction_output_tokens: None,
            agent_id,
            transaction_id: Some(transaction_id.clone()),
            cut: Some(tau_proto::AgentHead::Root),
            suffix_end: Some(suffix_end),
            compact_prompt_id: Some(compact_prompt_id),
            model: Some("provider-b/model".into()),
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
    );
    drop(store);

    {
        let mut h = quiet_provider_harness_for_with_start_reason_and_storage_mode(
            "s2",
            &state,
            tau_proto::SessionStartReason::Initial,
            crate::HarnessStorageMode::Durable,
        )
        .expect("start warm harness");
        assert!(
            h.provider_runtime
                .model_info
                .contains_key(&"test/model".into()),
            "provider state is populated before warm resume"
        );
        h.switch_session(test_session_id("s1"), tau_proto::SessionStartReason::Resume)
            .expect("warm resume success cut");
        assert_eq!(
            h.session_runtime
                .agent_store
                .agent_events("main")
                .expect("agent events")
                .iter()
                .filter(|entry| matches!(
                    entry.event,
                    Event::AgentInferenceDispatchStarted(
                        tau_proto::AgentInferenceDispatchStarted {
                            output_length_continuation: None,
                            transaction_id: Some(_),
                            ..
                        }
                    )
                ))
                .count(),
            1,
            "authoritative session initialization must checkpoint the exact missing-model work before terminalizing it"
        );
        let checkpoint = h
            .session_runtime
            .agent_store
            .agent_events("main")
            .expect("agent events")
            .iter()
            .find_map(|entry| match &entry.event {
                Event::AgentInferenceDispatchStarted(checkpoint)
                    if checkpoint.transaction_id.is_some() =>
                {
                    Some(checkpoint.clone())
                }
                _ => None,
            })
            .expect("qualified terminal checkpoint");
        assert_eq!(checkpoint.model, Some("provider-b/model".into()));
        assert_eq!(checkpoint.activation_cut, Some(tau_proto::AgentHead::Root));
        assert_eq!(
            checkpoint.operation,
            Some(tau_proto::PromptOperation::Inference)
        );
        assert!(
            h.session_runtime
                .agent_store
                .agent("main")
                .expect("agent")
                .contains_head_ancestry(
                    checkpoint.activation_cut.expect("activation cut"),
                    checkpoint.through,
                ),
            "checkpoint watermark must remain on the captured compacted branch"
        );
        assert_eq!(checkpoint.transaction_id.as_ref(), Some(&transaction_id));
        assert!(
            h.session_runtime
                .agent_store
                .agent_events("main")
                .expect("events")
                .iter()
                .any(|entry| matches!(
                    &entry.event,
                    Event::ProviderResponseFinished(response)
                        if response.agent_prompt_id == checkpoint.agent_prompt_id
                            && response.stop_reason == tau_proto::ProviderStopReason::Error
                            && response.failure_kind
                                == Some(tau_proto::ProviderFailureKind::Unknown)
                ))
        );
        let cid = h.agent_runtime.agent_registry.agent_routes["main"].clone();
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .identity
            .model_override = Some("provider-b/model".into());
        let owner = h.provider_runtime.model_routes[&"test/model".into()].to_string();
        let mut model = h.provider_runtime.model_info[&"test/model".into()].clone();
        model.id = "provider-b/model".into();
        h.publish_provider_models_update(
            &crate::test_connection_id(&owner),
            crate::test_extension_name(owner.clone()),
            tau_proto::ProviderModelsDeclared {
                models: vec![model],
            },
        );
        assert_eq!(
            h.session_runtime
                .agent_store
                .agent_events("main")
                .expect("agent events")
                .iter()
                .filter(|entry| matches!(
                    entry.event,
                    Event::AgentInferenceDispatchStarted(
                        tau_proto::AgentInferenceDispatchStarted {
                            output_length_continuation: None,
                            transaction_id: Some(_),
                            ..
                        }
                    )
                ))
                .count(),
            1
        );
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::AgentPromptCreated(prompt)
                        if prompt.operation == tau_proto::PromptOperation::Inference
                ))
                .count(),
            0,
            "later discovery must not resend work that was durably terminalized as unavailable"
        );
        h.switch_session(test_session_id("s2"), tau_proto::SessionStartReason::New)
            .expect("switch away");
        h.switch_session(test_session_id("s1"), tau_proto::SessionStartReason::Resume)
            .expect("warm resume with provider state already populated");
        assert_eq!(
            h.session_runtime
                .agent_store
                .agent_events("main")
                .expect("warm resumed events")
                .iter()
                .filter(|entry| matches!(
                    entry.event,
                    Event::AgentInferenceDispatchStarted(
                        tau_proto::AgentInferenceDispatchStarted {
                            output_length_continuation: None,
                            transaction_id: Some(_),
                            ..
                        }
                    )
                ))
                .count(),
            1,
            "warm resume must not duplicate terminalized continuation ownership"
        );
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("second resume");
    assert!(!event_log_events(&resumed).iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.operation == tau_proto::PromptOperation::Inference
    )));
    resumed.shutdown().expect("shutdown");
}

/// Facts committed while reactive compaction is pending stay in the suffix,
/// while the durable start retains the original pre-activation cut.
#[test]
fn reactive_context_overflow_preserves_earliest_cut_and_suffix() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: agent_id.clone(),
            inference_activation: false,
            text: "stable prefix".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let prefix = tau_proto::AgentHead::Node(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("stable prefix"),
    );
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("activation A".to_owned()))
        .expect("dispatch inference");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("start recovery");
    let compact = read_nth_prompt_created(&h, 1);
    let start = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .expect("start");
    assert_eq!(start.cut, prefix);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: crate::parse_agent_id(&agent_id),
            inference_activation: false,
            text: "suffix B".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(
        strict_fake_compact_response(&compact)
            .expect("strict fake provider accepts the corrected mixed-round cut"),
    )
    .expect("accept compact");
    let continuation = read_nth_prompt_created(&h, 2);
    let context = serde_json::to_string(&continuation.context).expect("context");
    assert_eq!(context.matches("activation A").count(), 1);
    assert_eq!(context.matches("suffix B").count(), 1);
    h.shutdown().expect("shutdown");
}

/// A reactive rejection must retain the checkpoint cut before the earliest of
/// multiple coalesced agent-message wakes and replay both wakes in the exact
/// suffix.
#[test]
fn reactive_compaction_cuts_before_earliest_coalesced_agent_message_wake() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: crate::parse_agent_id(&agent_id),
            text: "stable prefix".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let prefix = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("prefix");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("busy-before-wakes"),
    };
    for (message_id, body) in [
        ("coalesced-message-one", "coalesced body one"),
        ("coalesced-message-two", "coalesced body two"),
    ] {
        h.publish_event(
            Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
            Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
                message_id: tau_proto::AgentMessageId::parse(message_id)
                    .expect("test identifier must satisfy its grammar"),
                sender_id: crate::parse_agent_id("manager"),
                sender_session_id: None,
                recipient_id: crate::parse_agent_id(&agent_id),
                kind: tau_proto::AgentMessageKind::Message,
                watch_provider_status: None,
                watch_work_status: None,
                watch_long_wait: None,
                watch_lifecycle: None,
                message: body.to_owned(),
            }),
        );
    }
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_message_wakes
            .len(),
        2
    );
    let captured_cut = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("last message wake");
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: true,
            agent_id: crate::parse_agent_id(&agent_id),
            text: "mixed activation after coalesced wakes".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let through = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("second wake head");
    h.set_agent_turn_state(&cid, AgentTurnState::Idle);
    h.drain_publish_idle_dispatches();
    h.try_advance_queue();
    let inference = read_nth_prompt_created(&h, 0);
    let checkpoint = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentInferenceDispatchStarted(checkpoint)
                if checkpoint.agent_prompt_id == inference.agent_prompt_id =>
            {
                Some(checkpoint)
            }
            _ => None,
        })
        .expect("activation checkpoint");
    assert_eq!(
        checkpoint.activation_cut,
        Some(tau_proto::AgentHead::Node(prefix))
    );
    assert_ne!(
        checkpoint.activation_cut,
        Some(tau_proto::AgentHead::Node(captured_cut))
    );
    assert_eq!(checkpoint.through, tau_proto::AgentHead::Node(through));

    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("start reactive compaction");
    let compact = read_nth_prompt_created(&h, 1);
    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .expect("reactive compaction start");
    assert_eq!(started.cut, tau_proto::AgentHead::Node(prefix));
    h.handle_provider_response_finished(
        strict_fake_compact_response(&compact).expect("valid compact response"),
    )
    .expect("finish compaction");
    let continuation = read_nth_prompt_created(&h, 2);
    let context = serde_json::to_string(&continuation.context).expect("context");
    assert_eq!(context.matches("coalesced body one").count(), 1);
    assert_eq!(context.matches("coalesced body two").count(), 1);
    assert_eq!(
        context
            .matches("mixed activation after coalesced wakes")
            .count(),
        1
    );
    h.shutdown().expect("shutdown");
}
/// Incoming user work preempts a non-tool extension's reactive compact through
/// the production preemption path and durably cancels it exactly once.
#[test]
fn reactive_context_overflow_extension_preemption_cancels_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    h.handle_start_agent_request(
        &crate::test_connection_id(HARNESS_CONNECTION_ID),
        ext_query("q-preempt-reactive"),
    )
    .expect("start extension side agent");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("start recovery");
    let compact = read_nth_prompt_created(&h, 1);
    h.submit_user_prompt(test_session_id("s1"), "preempt side work".to_owned())
        .expect("submit user prompt");
    h.handle_provider_response_finished(context_overflow_response(&compact))
        .expect("ignore late compact response");
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.reason == tau_proto::StandaloneCompactionFailureReason::Cancelled
            ))
            .count(),
        1
    );
    let side_cid = ext_query_cid(&h, "q-preempt-reactive").expect("side agent retained");
    assert!(
        h.agent_runtime.agent_registry.agents[&side_cid]
            .dispatch
            .in_flight_prompt
            .is_none()
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&side_cid]
            .turn
            .turn_state,
        AgentTurnState::Idle
    ));
    h.shutdown().expect("shutdown");
}

/// Tool-backed delegate cancellation terminalizes an in-flight reactive
/// compact once, ignores its late response, and completes the parent request.
#[test]
fn reactive_context_overflow_delegate_cancel_is_terminal_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    let call_id = ToolCallId::from("delegate-reactive-call");
    let parent = ensure_test_user_agent(&mut h);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), parent);
    let mut query = ext_query("q-delegate-reactive");
    query.tool_call_id = Some(call_id.clone());
    h.handle_start_agent_request(&crate::test_connection_id(HARNESS_CONNECTION_ID), query)
        .expect("start delegated agent");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("start recovery");
    let compact = read_nth_prompt_created(&h, 1);
    h.cancel_start_agent_request("q-delegate-reactive", &call_id, false)
        .expect("cancel delegate");
    h.handle_provider_response_finished(context_overflow_response(&compact))
        .expect("ignore late compact response");
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.reason == tau_proto::StandaloneCompactionFailureReason::Cancelled
            ))
            .count(),
        1
    );
    assert!(ext_query_cid(&h, "q-delegate-reactive").is_none());
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::StartAgentResult(result)
            if result.query_id == "q-delegate-reactive" && result.error.is_some()
    )));
    h.shutdown().expect("shutdown");
}

/// Switching sessions terminalizes reactive compaction and clears all
/// old-session semantic-output, routing, and recovery watcher state.
#[test]
fn reactive_context_overflow_session_switch_cancels_and_cleans_state() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    seed_reactive_compaction_prefix(&mut h, &cid);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow".to_owned()))
        .expect("dispatch inference");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("start recovery");
    let compact = read_nth_prompt_created(&h, 1);
    h.prompt_coordination
        .prompt_runtime
        .semantic_output
        .insert(compact.agent_prompt_id.clone());
    h.switch_session(test_session_id("s2"), tau_proto::SessionStartReason::New)
        .expect("switch session");
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.reason == tau_proto::StandaloneCompactionFailureReason::Cancelled
            ))
            .count(),
        1
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .semantic_output
            .is_empty()
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .agents
            .contains_key(&compact.agent_prompt_id)
    );
    assert!(
        !h.agent_runtime
            .agent_watch
            .provider_status
            .contains_key(&agent_id)
    );
    h.handle_provider_response_finished(context_overflow_response(&compact))
        .expect("late old-session response is ignored");
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentStandaloneCompactionStarted(started)
                if started.compact_prompt_id != compact.agent_prompt_id
                    && matches!(started.trigger, tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. })))
    );
    h.shutdown().expect("shutdown");
}

/// Explicit navigation invalidates an in-flight compact response even if the
/// selected head value is unchanged and the cut remains an ancestor.
#[test]
fn standalone_compaction_rejects_response_after_head_navigation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let compact = read_nth_prompt_created(&h, 0);
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: crate::parse_agent_id(&agent_id),
            head: h.agent_runtime.agent_registry.agents[&cid]
                .identity
                .head
                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
        }),
    );
    let mut stale_response =
        provider_text_response(&compact.agent_prompt_id, compact.agent_id, "stale summary");
    stale_response.usage = Some(tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000_000,
        ..tau_proto::ProviderTokenUsage::default()
    });
    h.handle_provider_response_finished(stale_response)
        .expect("handle stale response");

    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.reason
                        == tau_proto::StandaloneCompactionFailureReason::StaleBranch
            ))
            .count(),
        1
    );
    assert!(
        !event_log_events(&h)
            .into_iter()
            .any(|event| matches!(event, Event::AgentCompacted(_)))
    );
    assert_eq!(
        h.agent_stats_snapshot(&cid)
            .expect("agent stats")
            .estimated_api_cost,
        tau_proto::EstimatedApiCost::default(),
        "a stale terminal must not charge the agent"
    );
    h.shutdown().expect("shutdown");
}

/// A terminal UI compaction failure suppresses hidden automatic retries but
/// leaves ordinary prompt dispatch and explicit successor retries available.
#[test]
fn standalone_compaction_failure_does_not_retry_automatically() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(1));
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let compact = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: compact.agent_prompt_id,
        agent_id: crate::parse_agent_id(&agent_id),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: Some("secret provider detail".to_owned()),
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("record terminal compact failure");
    h.try_advance_queue();
    h.try_advance_queue();

    assert!(
        matches!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .activation_dispatch,
            crate::agent::ActivationDispatchState::None
        ),
        "unexpected activation state: {:?}",
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch
    );
    assert!(
        !h.agent_runtime
            .agent_watch
            .provider_status
            .get(agent_id.as_str())
            .is_some_and(|status| matches!(
                status.state,
                tau_proto::AgentWatchProviderState::Blocked { .. }
            ))
    );
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(event, Event::AgentStandaloneCompactionFailed(_)))
            .count(),
        1
    );
    {
        let agent = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        agent.execution.context_input_tokens = Some(1_000);
        agent.execution.context_usage_model = Some("test/model".into());
        agent.execution.context_usage_prompt_id =
            Some(test_agent_prompt_id("ap-test-provider-usage"));
        agent.execution.context_usage_head = agent.identity.head;
    }
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        1
    );
    assert!(!event_log_events(&h).into_iter().any(|event| {
        matches!(event, Event::AgentStandaloneCompactionFailed(failed)
            if serde_json::to_string(&failed).expect("serialize failure").contains("secret provider detail"))
    }));
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: true,
            agent_id: crate::parse_agent_id(&agent_id),
            text: "prompt after failed UI compaction".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    h.drain_publish_idle_dispatches();
    h.try_advance_queue();
    let inference = read_nth_prompt_created(&h, 1);
    assert_eq!(inference.operation, tau_proto::PromptOperation::Inference);
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        1,
        "same-model/branch threshold scheduling must use durable suppression"
    );
    h.handle_provider_response_finished(provider_text_response(
        &inference.agent_prompt_id,
        inference.agent_id,
        "ordinary inference remains usable",
    ))
    .expect("finish ordinary inference");
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let starts: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(
        starts[1].supersedes.as_ref(),
        Some(&starts[0].transaction_id)
    );
    assert_ne!(starts[1].transaction_id, starts[0].transaction_id);
    let successor = read_nth_prompt_created(&h, 2);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: successor.agent_prompt_id,
        agent_id: crate::parse_agent_id(&agent_id),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: Some("second provider detail".to_owned()),
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("record successor failure");
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
}

/// Every rejected standalone terminal must preserve the pre-existing context
/// baseline and durable transaction authority while recording one typed
/// A canonical provider compaction item must become the durable replacement
/// window unchanged so later replay can return it to the provider.
#[test]
fn standalone_compaction_accepts_canonical_opaque_provider_item() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let compact = read_nth_prompt_created(&h, 0);
    let replacement = ContextItem::Compaction(
        tau_proto::OpaqueProviderItem::from_raw_json(
            r#"{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}"#,
        )
        .expect("valid compaction item"),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: compact.agent_prompt_id,
        agent_id: crate::parse_agent_id(&agent_id),
        output_items: vec![replacement.clone()],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
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
    .expect("accept canonical compaction");

    assert!(event_log_events(&h).iter().any(|event| {
        matches!(event, Event::AgentCompacted(compacted)
            if compacted.replacement_window == vec![replacement.clone()])
    }));
    h.shutdown().expect("shutdown");
}

/// Cold replay restores nonblocking suppression without projecting the failed
/// UI operation as active provider work.
#[test]
fn failed_ui_compaction_replay_restores_nonblocking_suppression() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let agent_id;
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        let cid = ensure_test_user_agent(&mut h);
        agent_id = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .clone()
            .expect("durable agent");
        h.handle_compact_request(
            crate::harness::harness_connection_id(),
            test_session_id("s1"),
            Some(&agent_id),
        );
        let compact = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: compact.agent_prompt_id,
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: Vec::new(),
            stop_reason: tau_proto::ProviderStopReason::Error,
            error: Some("raw compact failure".to_owned()),
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: tau_proto::PromptOriginator::User,
            usage: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        })
        .expect("record compact failure");
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert!(
        !resumed
            .agent_runtime
            .agent_watch
            .provider_status
            .contains_key(&agent_id)
    );
    let recovered = resumed
        .agent_runtime
        .agent_registry
        .agents
        .values()
        .find(|agent| agent.identity.agent_id.as_deref() == Some(agent_id.as_str()))
        .expect("replayed agent");
    assert!(matches!(
        recovered.dispatch.activation_dispatch,
        ActivationDispatchState::None
    ));
    assert!(
        resumed
            .prompt_coordination
            .compaction_runtime
            .active_manual_starts_is_empty(),
        "a terminal UI transaction must not reappear as active after replay"
    );
    resumed.shutdown().expect("shutdown");
}

/// Runtime compaction ownership includes the target agent because transaction
/// ids are only unique within one agent journal.
#[test]
fn restored_ui_transactions_with_equal_local_ids_remain_agent_scoped() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let first_cid = ensure_test_user_agent(&mut h);
    let second_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let first_id = durable_agent_id_for_conversation(&h, &first_cid);
    let second_id = durable_agent_id_for_conversation(&h, &second_cid);
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-agent-local").expect("transaction id");
    let recovery = |cid: AgentId, agent_id: tau_proto::AgentId, ordinal: &str| {
        let request_id =
            tau_proto::CompactionRequestId::parse(format!("cr-{ordinal}")).expect("request id");
        let request = tau_proto::AgentManualCompactionRequested {
            request_id: request_id.clone(),
            target_agent_id: agent_id.clone(),
            source: tau_proto::ManualCompactionSource::UiCompact {
                ui_compact: tau_proto::UiManualCompactionSource {
                    eligible_automatic_transaction_id: None,
                    target_role: h.config.selected_role.clone(),
                },
            },
            requested_target_head: tau_proto::AgentHead::Root,
            target_generation: tau_proto::MaterializedPromptGeneration::initial(),
            model: "test/model".into(),
        };
        (
            cid,
            tau_core::ManualCompactionRecovery::Started {
                requested: request,
                started: Box::new(tau_proto::AgentStandaloneCompactionStarted {
                    agent_id,
                    transaction_id: transaction_id.clone(),
                    compact_prompt_id: test_agent_prompt_id(format!("ap-{ordinal}")),
                    cut: tau_proto::AgentHead::Root,
                    resume_through: None,
                    model: "test/model".into(),
                    operation: tau_proto::PromptOperation::StandaloneCompaction,
                    originator: tau_proto::PromptOriginator::User,
                    supersedes: None,
                    trigger: tau_proto::StandaloneCompactionTrigger::ManualUi { request_id },
                }),
                outcome: None,
            },
        )
    };

    h.restore_manual_compaction_tools(vec![
        recovery(first_cid, first_id.clone(), "first"),
        recovery(second_cid, second_id.clone(), "second"),
    ]);

    assert_eq!(
        h.prompt_coordination
            .compaction_runtime
            .active_manual_start_count(),
        2
    );
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .has_ui_start(first_id, transaction_id.clone())
    );
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .has_ui_start(second_id, transaction_id)
    );
    h.shutdown().expect("shutdown");
}

/// A terminal for one agent-local transaction must neither remove nor complete
/// another agent's model-tool transaction with the same local id.
#[test]
fn model_tool_terminal_with_equal_local_id_keeps_other_agent_owner() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let first_cid = ensure_test_user_agent(&mut h);
    let second_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let first_id = durable_agent_id_for_conversation(&h, &first_cid);
    let second_id = durable_agent_id_for_conversation(&h, &second_cid);
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-agent-local-tool").expect("transaction id");
    let second_call = ToolCallId::from("call-second-agent-local");
    for (agent_id, ordinal, call_id) in [
        (
            first_id.clone(),
            "first",
            ToolCallId::from("call-first-agent-local"),
        ),
        (second_id.clone(), "second", second_call.clone()),
    ] {
        h.prompt_coordination
            .compaction_runtime
            .record_model_tool_start(
                agent_id.clone(),
                transaction_id.clone(),
                crate::harness::PendingManualCompactionTool {
                    request_id: tau_proto::CompactionRequestId::parse(format!("cr-{ordinal}"))
                        .expect("request id"),
                    caller_agent_id: agent_id.clone(),
                    call_id,
                    tool_name: ToolName::new("compact"),
                    target_agent_id: agent_id,
                },
            );
    }

    h.react_to_committed_event(
        None,
        &Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
            agent_id: first_id.clone(),
            transaction_id: transaction_id.clone(),
            cut: tau_proto::AgentHead::Root,
            reason: tau_proto::StandaloneCompactionFailureReason::Interrupted,
            resume_through: None,
            context_retreat: None,
            incomplete_response: None,
        }),
        true,
        None,
    );

    assert!(
        !h.prompt_coordination
            .compaction_runtime
            .has_model_tool_start(first_id, transaction_id.clone())
    );
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .has_model_tool_start(second_id, transaction_id)
    );
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::ToolBackgroundError(error) if error.call_id == second_call
        )
    }));
    h.shutdown().expect("shutdown");
}
/// A large provider-usage value from another model cannot trigger protected
/// compaction when the selected model's active window is small.
#[test]
fn standalone_auto_compaction_ignores_stale_usage_baseline() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(10_000));
    let cid = ensure_test_user_agent(&mut h);
    let agent = h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent");
    agent.execution.context_input_tokens = Some(100_000);
    agent.execution.context_usage_model = Some("stale/model".into());
    agent.execution.context_usage_prompt_id = Some(test_agent_prompt_id("ap-test-provider-usage"));

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("small current turn".to_owned()))
        .expect("dispatch current turn");

    assert_eq!(
        read_nth_prompt_created(&h, 0).operation,
        tau_proto::PromptOperation::Inference
    );
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );
    h.shutdown().expect("shutdown");
}
/// Self-compaction envelopes expose only closed bounded status and exact
/// durable correlation for every terminal class. They must also stay outside
/// generic background-completion UI classification so their distinct diagnostic
/// projection cannot disappear with tool lifecycle notices.
#[test]
fn self_compaction_terminal_envelopes_are_literal_and_bounded() {
    let request_id = tau_proto::CompactionRequestId::parse("cr-envelope").expect("request");
    let call_id = ToolCallId::from("call-envelope");
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-envelope").expect("transaction");
    let cases = [
        (
            tau_proto::SelfCompactionTerminalOutcome::Compacted,
            Some(transaction_id.clone()),
            "Self compact terminal: {\"status\":\"compacted\",\"request_id\":\"cr-envelope\",\"tool_call_id\":\"call-envelope\",\"transaction_id\":\"ct-envelope\"}",
        ),
        (
            tau_proto::SelfCompactionTerminalOutcome::Failed {
                reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            },
            Some(transaction_id),
            "Self compact terminal: {\"status\":\"provider_error\",\"request_id\":\"cr-envelope\",\"tool_call_id\":\"call-envelope\",\"transaction_id\":\"ct-envelope\"}",
        ),
        (
            tau_proto::SelfCompactionTerminalOutcome::RequestFailed {
                reason: tau_proto::ManualCompactionRequestFailureReason::Cancelled,
            },
            None,
            "Self compact terminal: {\"status\":\"compaction_cancelled\",\"request_id\":\"cr-envelope\",\"tool_call_id\":\"call-envelope\",\"transaction_id\":null}",
        ),
    ];
    for (outcome, transaction_id, expected) in cases {
        let terminal = tau_proto::SelfCompactionTerminal {
            request_id: request_id.clone(),
            tool_call_id: call_id.clone(),
            transaction_id,
            outcome,
        };
        assert_eq!(self_compaction_terminal_prompt(&terminal), expected);
        assert_eq!(
            self_compaction_terminal_pending_prompt(terminal).internal_kind(),
            None,
            "self-compaction terminals are not generic background-tool completion notices"
        );
    }
}

#[test]
fn late_prompt_surface_failure_terminalizes_running_compaction() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-late-surface").expect("transaction id");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("user agent")
        .dispatch
        .activation_dispatch = path_crate_agent::ActivationDispatchState::Running {
        id: transaction_id.clone(),
        cut: tau_proto::AgentHead::Root,
        resume_through: None,
        model: "test/model".into(),
        branch_generation: 0,
        compact_prompt_id: test_agent_prompt_id("ap-late-surface"),
    };

    for internal_name in ["first_internal", "second_internal"] {
        h.tool_routing.registry.register(
            &crate::test_connection_id("late-surface-test"),
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

    assert!(h.prepare_agent_prompt_for_dispatch(&cid).is_none());
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentStandaloneCompactionFailed(failed)
            if failed.transaction_id == transaction_id
                && failed.reason
                    == tau_proto::StandaloneCompactionFailureReason::RouteFailed
    )));
    h.shutdown().expect("shutdown");
}
/// A restored standalone continuation rejects off-branch reconciliation
/// without an attempt marker, then retries after a journal append failure
/// without retrying on owning-branch reselection.
#[test]
fn standalone_checkpoint_storage_rejection_retries_after_recovery() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let mut h = quiet_provider_harness(&state_dir).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let (agent_id, transaction_id, prompt_id, through) =
        seed_restored_compaction_checkpoint(&mut h, &cid, &"test/model".into(), "ct-owner");
    let checkpoint =
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            output_length_continuation: None,
            agent_id: agent_id.clone(),
            transaction_id: Some(transaction_id.clone()),
            agent_prompt_id: prompt_id.clone(),
            through,
            model: Some("test/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
        });

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .identity
        .head = None;
    assert!(!h.activation_successor_matches_selected_head(&checkpoint));
    h.resume_restored_compaction_checkpoints(RestoredCheckpointAuthority::DiscoveryComplete);
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .enqueued_inference_checkpoints
            .is_empty()
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        0
    );
    h.rollback_rejected_activation_successor(&checkpoint);
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::AwaitingCheckpoint {
            owner: crate::agent::InferenceCheckpointOwner::Standalone { .. },
            ..
        }
    ));

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .identity
        .head = through.as_option();
    h.prompt_coordination
        .compaction_runtime
        .enqueued_inference_checkpoints
        .insert((agent_id.clone(), transaction_id.clone()));
    reject_next_semantic_admission(&h);
    h.publish_for_agent(&cid, checkpoint);
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::AwaitingCheckpoint {
            owner: crate::agent::InferenceCheckpointOwner::Standalone { .. },
            ..
        }
    ));
    assert!(
        !h.prompt_coordination
            .compaction_runtime
            .enqueued_inference_checkpoints
            .contains(&(agent_id.clone(), transaction_id.clone()))
    );

    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::AwaitingCheckpoint { .. }
    ));
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: through,
        }),
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(created) if created.agent_prompt_id == prompt_id
    )));
    let committed: Vec<_> = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent records")
        .into_iter()
        .filter_map(|record| match record.event {
            Event::AgentInferenceDispatchStarted(started)
                if started.transaction_id.as_ref() == Some(&transaction_id) =>
            {
                Some(started)
            }
            _ => None,
        })
        .collect();
    assert_eq!(committed.len(), 1);
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: through,
        }),
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        1
    );
}
/// Rewinding a large branch to root must discard its provider usage baseline,
/// so a tiny replacement branch and a fresh adjacent agent start with ordinary
/// inference instead of sending invalid near-empty standalone compactions.
#[test]
fn rewind_discards_off_branch_usage_before_tiny_and_new_agent_activations() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(10_000));
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id.clone(),
            text: "old large branch".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let old_branch = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("old branch head");
    {
        let agent = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        agent.execution.context_input_tokens = Some(10_000);
        agent.execution.context_cached_tokens = Some(5_000);
        agent.execution.context_usage_head = Some(old_branch);
        agent.execution.context_usage_model = Some("test/model".into());
        agent.execution.context_usage_prompt_id =
            Some(test_agent_prompt_id("ap-test-provider-usage"));
    }

    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: tau_proto::AgentHead::Root,
        }),
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        None
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_usage_head,
        None
    );
    assert!(event_log_events(&h).iter().rev().any(|event| {
        matches!(
            event,
            Event::HarnessAgentContextUsageChanged(usage)
                if usage.agent_id == cid
                    && usage.input_tokens.is_none()
                    && usage.cached_tokens.is_none()
                    && usage.context_window.is_none()
                    && usage.percent_used.is_none()
        )
    }));
    assert!(event_log_events(&h).iter().rev().any(|event| {
        matches!(event, Event::AgentStatsUpdated(stats) if stats.agent_id == cid)
    }));

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("tiny branch".to_owned()))
        .expect("dispatch tiny replacement branch");
    assert_eq!(
        read_nth_prompt_created(&h, 0).operation,
        tau_proto::PromptOperation::Inference
    );

    let fresh = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    h.dispatch_prompt_for_agent(&fresh, PendingPrompt::user("first tiny turn".to_owned()))
        .expect("dispatch fresh agent");
    assert_eq!(
        read_nth_prompt_created(&h, 1).operation,
        tau_proto::PromptOperation::Inference
    );
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| { !matches!(event, Event::AgentStandaloneCompactionStarted(_)) })
    );
    h.shutdown().expect("shutdown");
}

/// Cold replay projects inference owed by a successful standalone compaction as
/// compaction-owned uncertainty with the exact checkpoint prompt.
#[test]
fn standalone_dispatch_uncertain_replay_projects_compaction_category() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (agent_id, inference_prompt_id);
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
        let cid = ensure_test_user_agent(&mut h);
        establish_exact_provider_usage(&mut h, &cid, 900);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(900));
        agent_id = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .clone()
            .expect("durable agent");
        h.publish_for_agent(
            &cid,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: crate::parse_agent_id(&agent_id),
                text: "stable prefix ".repeat(80),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        );
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .execution
            .context_input_tokens = Some(900);
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .execution
            .context_usage_model = Some("test/model".into());
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("activation".to_owned()))
            .expect("start automatic compaction");
        let compact = read_nth_prompt_created(&h, 1);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
        h.handle_provider_response_finished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: compact.agent_prompt_id,
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "summary".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: tau_proto::PromptOriginator::User,
            usage: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        })
        .expect("accept compaction");
        inference_prompt_id = read_nth_prompt_created(&h, 2).agent_prompt_id;
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let status = &resumed.agent_runtime.agent_watch.provider_status[&agent_id];
    assert_eq!(status.agent_prompt_id, inference_prompt_id);
    assert!(matches!(
        status.state,
        tau_proto::AgentWatchProviderState::DispatchUncertain {
            category: tau_proto::AgentWatchProviderCategory::Compaction
        }
    ));
    resumed.shutdown().expect("shutdown");
}

/// A failed automatic transaction retains A across an explicit retry, while B
/// committed during recovery joins the one eventual inference exactly once.
#[test]
fn standalone_compaction_retry_preserves_owed_and_later_activations() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    let cid = ensure_test_user_agent(&mut h);
    establish_exact_provider_usage(&mut h, &cid, 900);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(900));
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: crate::parse_agent_id(&agent_id),
            text: "stable prefix ".repeat(80),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .context_input_tokens = Some(900);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .context_usage_model = Some("test/model".into());

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("activation A".to_owned()))
        .expect("start automatic compact");
    let first_compact = read_nth_prompt_created(&h, 1);
    let mut failed = provider_text_response(
        &first_compact.agent_prompt_id,
        first_compact.agent_id,
        "ignored",
    );
    failed.output_items.clear();
    failed.error = Some("provider failed".to_owned());
    h.handle_provider_response_finished(failed)
        .expect("record failure");

    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: crate::parse_agent_id(&agent_id),
            text: "activation B".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let retry_compact = read_nth_prompt_created(&h, 2);
    h.handle_provider_response_finished(provider_text_response(
        &retry_compact.agent_prompt_id,
        retry_compact.agent_id,
        "replacement",
    ))
    .expect("accept retry");

    let inference = read_nth_prompt_created(&h, 3);
    let context = serde_json::to_string(&inference.context).expect("context");
    assert_eq!(context.matches("activation A").count(), 1);
    assert_eq!(context.matches("activation B").count(), 1);
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::Inference
            ))
            .count(),
        2
    );
    h.shutdown().expect("shutdown");
}

/// A threshold reached immediately after a mixed parallel tool round must
/// compact only a closed provider prefix and preserve every success, error, and
/// cancellation result exactly in the one resumed inference.
#[test]
fn standalone_auto_compaction_keeps_complete_mixed_tool_round_in_suffix() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    let cid = ensure_test_user_agent(&mut h);
    establish_exact_provider_usage(&mut h, &cid, 900);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(900));
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");

    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: crate::parse_agent_id(&agent_id),
            text: "prefix".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let prefix = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("prefix head");
    let calls = [
        (
            "call-success",
            "success_tool",
            tau_proto::ToolType::Function,
        ),
        ("call-error", "error_tool", tau_proto::ToolType::Custom),
        ("call-cancel", "cancel_tool", tau_proto::ToolType::Function),
    ];
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: test_agent_prompt_id("ap-mixed-round"),
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: calls
                .iter()
                .map(|(call_id, name, tool_type)| {
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: (*call_id).into(),
                        name: ToolName::new(*name),
                        tool_type: *tool_type,
                        arguments: CborValue::Map(Vec::new()),
                        raw_arguments_json: None,
                        responses_envelope: None,
                    })
                })
                .collect(),
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
        }),
    );
    let assistant = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("assistant head");
    h.publish_for_agent(
        &cid,
        Event::ProviderToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-success".into(),
            tool_name: ToolName::new("success_tool"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("success output".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderToolError(tau_proto::ToolError {
            presentation: Default::default(),
            call_id: "call-error".into(),
            tool_name: ToolName::new("error_tool"),
            tool_type: tau_proto::ToolType::Custom,
            message: "expected failure".to_owned(),
            details: None,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::ToolCancelled(tau_proto::ToolCancelled {
            presentation: Default::default(),
            call_id: "call-cancel".into(),
            tool_name: ToolName::new("cancel_tool"),
            tool_type: tau_proto::ToolType::Function,
            display: None,
        }),
    );
    let results = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("results head");
    assert_ne!(results, assistant);

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .context_input_tokens = Some(900);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .context_usage_model = Some("test/model".into());
    assert!(h.schedule_standalone_auto_compaction_for_activation(&cid, true, None));

    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .expect("compaction start");
    assert_eq!(started.cut, tau_proto::AgentHead::Node(prefix));
    assert_eq!(
        started.resume_through,
        Some(tau_proto::AgentHead::Node(results))
    );
    let compact = read_nth_prompt_created(&h, 1);
    assert!(
        compact
            .context
            .flatten_iter()
            .all(|item| !matches!(item, ContextItem::ToolCall(_) | ContextItem::ToolResult(_))),
        "compact input must end before the complete mixed tool round"
    );
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));

    h.handle_provider_response_finished(
        strict_fake_compact_response(&compact)
            .expect("strict fake provider accepts the corrected mixed-round cut"),
    )
    .expect("accept compaction");
    let inference = read_nth_prompt_created(&h, 2);
    let timeline: Vec<_> = inference.context.flatten_iter().collect();
    let call_types: std::collections::HashMap<_, _> = timeline
        .iter()
        .filter_map(|item| match item {
            ContextItem::ToolCall(call) => Some((call.call_id.clone(), call.tool_type)),
            _ => None,
        })
        .collect();
    let result_types: std::collections::HashMap<_, _> = timeline
        .iter()
        .filter_map(|item| match item {
            ContextItem::ToolResult(result) => Some((result.call_id.clone(), result.tool_type)),
            _ => None,
        })
        .collect();
    assert_eq!(call_types, result_types);
    assert_eq!(call_types.len(), 3);
    let results_by_id: std::collections::HashMap<_, _> = timeline
        .iter()
        .filter_map(|item| match item {
            ContextItem::ToolResult(result) => Some((result.call_id.as_str(), result)),
            _ => None,
        })
        .collect();
    assert!(matches!(
        results_by_id["call-success"].status,
        ToolResultStatus::Success
    ));
    assert_eq!(
        results_by_id["call-success"].output.render(),
        "success output"
    );
    assert!(matches!(
        results_by_id["call-error"].status,
        ToolResultStatus::Error { ref message } if message == "expected failure"
    ));
    assert!(matches!(
        results_by_id["call-cancel"].status,
        ToolResultStatus::Cancelled { ref reason } if reason == "cancelled"
    ));
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::Inference
            ))
            .count(),
        2
    );
    h.shutdown().expect("shutdown");
}
