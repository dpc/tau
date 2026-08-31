use super::*;

/// Branch drift permits an unrelated automatic transaction, but returning to
/// the failed branch restores suppression across that success and cold replay;
/// only a successful exact successor clears it.
#[test]
fn automatic_suppression_survives_unrelated_branch_success_and_cold_replay() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let agent_id;
    let failed_head;
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
        info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(1));
        let cid = ensure_test_user_agent(&mut h);
        agent_id = durable_agent_id_for_conversation(&h, &cid);
        let (_, _, results) = seed_historical_open_prefix_failure(&mut h, &cid, "test/model");
        failed_head = results;

        h.publish_for_agent(
            &cid,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: agent_id.clone(),
                head: tau_proto::AgentHead::Root,
            }),
        );
        h.publish_for_agent(
            &cid,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: agent_id.clone(),
                text: "independent sibling".to_owned(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        );
        {
            let agent = h
                .agent_runtime
                .agent_registry
                .agents
                .get_mut(&cid)
                .expect("agent");
            agent.execution.context_input_tokens = Some(tau_proto::TokenCount::new(1_000));
            agent.execution.context_usage_model = Some("test/model".into());
            agent.execution.context_usage_prompt_id =
                Some(test_agent_prompt_id("ap-test-provider-usage"));
            agent.execution.context_usage_head = agent.identity.head;
        }
        assert!(h.schedule_standalone_auto_compaction(&cid));
        let unrelated = read_nth_prompt_created(&h, 0);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .standalone_compaction_threshold = None;
        h.handle_provider_response_finished(provider_text_response(
            &unrelated.agent_prompt_id,
            unrelated.agent_id,
            "sibling summary",
        ))
        .expect("finish unrelated sibling compaction");
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(1));

        h.publish_for_agent(
            &cid,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: agent_id.clone(),
                head: failed_head,
            }),
        );
        {
            let agent = h
                .agent_runtime
                .agent_registry
                .agents
                .get_mut(&cid)
                .expect("agent");
            agent.execution.context_input_tokens = Some(tau_proto::TokenCount::new(1_000));
            agent.execution.context_usage_model = Some("test/model".into());
            agent.execution.context_usage_prompt_id =
                Some(test_agent_prompt_id("ap-test-provider-usage"));
            agent.execution.context_usage_head = agent.identity.head;
        }
        assert!(!h.schedule_standalone_auto_compaction(&cid));
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    enable_remote_compaction_for_test_model(&mut resumed);
    let info = resumed
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(1));
    let cid = resumed
        .runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
        .expect("restored agent");
    {
        let agent = resumed
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        assert_eq!(
            agent.identity.head.map(tau_proto::AgentHead::Node),
            failed_head.as_option().map(tau_proto::AgentHead::Node)
        );
        agent.execution.context_input_tokens = Some(tau_proto::TokenCount::new(1_000));
        agent.execution.context_usage_model = Some("test/model".into());
        agent.execution.context_usage_prompt_id =
            Some(test_agent_prompt_id("ap-test-provider-usage"));
        agent.execution.context_usage_head = agent.identity.head;
    }
    assert!(!resumed.schedule_standalone_auto_compaction(&cid));
    resumed.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    let successor = read_nth_prompt_created(&resumed, 0);
    resumed
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .standalone_compaction_threshold = None;
    resumed
        .handle_provider_response_finished(provider_text_response(
            &successor.agent_prompt_id,
            successor.agent_id,
            "exact successor",
        ))
        .expect("finish exact successor");
    let resumed_inference = read_nth_prompt_created(&resumed, 1);
    resumed
        .handle_provider_response_finished(provider_text_response(
            &resumed_inference.agent_prompt_id,
            resumed_inference.agent_id,
            "owed inference complete",
        ))
        .expect("finish owed inference");
    resumed
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(1));
    {
        let agent = resumed
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        agent.execution.context_input_tokens = Some(tau_proto::TokenCount::new(1_000));
        agent.execution.context_usage_model = Some("test/model".into());
        agent.execution.context_usage_prompt_id =
            Some(test_agent_prompt_id("ap-test-provider-usage"));
        agent.execution.context_usage_head = agent.identity.head;
    }
    assert!(resumed.schedule_standalone_auto_compaction(&cid));
    resumed.shutdown().expect("shutdown");
}

/// Reactive overflow planning uses the same durable model-and-branch failure
/// authority as threshold scheduling: an exact match persists no plan, while
/// either branch or provider-qualified-model drift remains eligible.
#[test]
fn reactive_recovery_respects_durable_failure_model_and_branch() {
    #[derive(Clone, Copy, Debug)]
    enum Case {
        Matching,
        BranchDrift,
        ModelDrift,
    }

    for case in [Case::Matching, Case::BranchDrift, Case::ModelDrift] {
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
        seed_historical_open_prefix_failure(&mut h, &cid, "test/model");

        if matches!(case, Case::BranchDrift) {
            h.publish_for_agent(
                &cid,
                Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                    agent_id: agent_id.clone(),
                    head: tau_proto::AgentHead::Root,
                }),
            );
            h.publish_for_agent(
                &cid,
                Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                    inference_activation: false,
                    agent_id: agent_id.clone(),
                    text: "independent reactive branch".to_owned(),
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
        if matches!(case, Case::ModelDrift) {
            let mut other = h
                .provider_runtime
                .model_info
                .get(&"test/model".into())
                .expect("test model")
                .clone();
            other.id = "other/model".into();
            h.provider_runtime
                .model_info
                .insert(other.id.clone(), other);
            let route = h
                .provider_runtime
                .model_routes
                .get(&"test/model".into())
                .cloned()
                .expect("test route");
            h.provider_runtime
                .model_routes
                .insert("other/model".into(), route);
            h.config.selected_model = Some("other/model".into());
        }
        if !matches!(case, Case::Matching) {
            let agent = &h.agent_runtime.agent_registry.agents[&cid];
            let selected_model = h.model_for_agent_role(agent).expect("selected model");
            let current_head = agent
                .identity
                .head
                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
            assert!(
                !h.durable_recovery_blocks_automatic(
                    agent_id.as_str(),
                    &selected_model,
                    current_head
                ),
                "drift must make the old failure inapplicable: {case:?}"
            );
        }

        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow".to_owned()))
            .expect("dispatch inference");
        let inference = read_nth_prompt_created(&h, 1);
        h.handle_provider_response_finished(context_overflow_response(&inference))
            .expect("handle overflow");
        let starts = event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
            .count();
        if matches!(case, Case::Matching) {
            assert_eq!(starts, 1, "matching failure must not persist a new plan");
            assert_eq!(
                event_log_events(&h)
                    .into_iter()
                    .filter(|event| matches!(
                        event,
                        Event::AgentPromptCreated(prompt)
                            if prompt.operation
                                == tau_proto::PromptOperation::StandaloneCompaction
                    ))
                    .count(),
                1
            );
        } else {
            let finished = event_log_events(&h)
                .into_iter()
                .find_map(|event| match event {
                    Event::ProviderResponseFinished(finished)
                        if finished.agent_prompt_id == inference.agent_prompt_id =>
                    {
                        Some(finished)
                    }
                    _ => None,
                });
            assert_eq!(
                starts, 2,
                "drift must preserve reactive eligibility: {case:?}, terminal={finished:?}"
            );
            assert_eq!(
                read_nth_prompt_created(&h, 2).operation,
                tau_proto::PromptOperation::StandaloneCompaction
            );
        }
        h.shutdown().expect("shutdown");
    }
}

/// A legacy replay log may contain a reactive plan committed before durable
/// failure suppression was enforced. Cold reconciliation claims it once with a
/// local terminal failure, never dispatches provider work, and a second restart
/// adds nothing.
#[test]
fn reactive_replay_terminalizes_plan_suppressed_by_prior_failure_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let agent_id;
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .supports_standalone_compaction = true;
        let cid = ensure_test_user_agent(&mut h);
        agent_id = durable_agent_id_for_conversation(&h, &cid);
        seed_historical_open_prefix_failure(&mut h, &cid, "test/model");
        let through = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .map(tau_proto::AgentHead::Node)
            .expect("head");
        let prompt_id = test_agent_prompt_id("ap-legacy-suppressed-reactive");
        h.session_runtime
            .agent_store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                tau_core::AgentEventParent::InheritHead,
                Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                    output_length_continuation: None,
                    agent_id: agent_id.clone(),
                    transaction_id: None,
                    agent_prompt_id: prompt_id.clone(),
                    through,
                    model: Some("test/model".into()),
                    operation: Some(tau_proto::PromptOperation::Inference),
                    activation_cut: Some(tau_proto::AgentHead::Root),
                }),
                tau_proto::UnixMicros::now(),
            )
            .expect("append legacy checkpoint");
        h.session_runtime
            .agent_store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                tau_core::AgentEventParent::InheritHead,
                Event::ProviderResponseFinished(ProviderResponseFinished {
                    automatic_compaction_decision: None,
                    output_length_disposition: tau_proto::OutputLengthDisposition::None,
                    estimated_api_cost_rates: None,
                    estimated_api_cost_increment: None,
                    agent_prompt_id: prompt_id,
                    agent_id: agent_id.clone(),
                    output_items: Vec::new(),
                    stop_reason: tau_proto::ProviderStopReason::Error,
                    error: Some("legacy planned overflow".to_owned()),
                    failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
                    context_limit_telemetry: None,
                    recovery_disposition:
                        tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned,
                    originator: tau_proto::PromptOriginator::User,
                    usage: None,
                    compaction_original_input_tokens: None,
                    compaction_output_tokens: None,
                    backend: None,
                    provider_attempt: Default::default(),
                    provider_response_id: None,
                    ws_pool_delta: None,
                }),
                tau_proto::UnixMicros::now(),
            )
            .expect("append legacy plan");
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    enable_remote_compaction_for_test_model(&mut resumed);
    resumed.reconcile_pending_context_recoveries(true);
    resumed.drain_publish_idle_dispatches();
    let records = loaded_agent_events(&resumed, "s1");
    let local_starts = records
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        started.trigger,
                        tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
                    )
            )
        })
        .count();
    let local_failures = records
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.reason == tau_proto::StandaloneCompactionFailureReason::StaleBranch
            )
        })
        .count();
    assert_eq!(local_starts, 1);
    assert_eq!(local_failures, 1);
    assert!(
        !event_log_events(&resumed).into_iter().any(|event| matches!(
            event,
            Event::AgentPromptCreated(prompt)
                if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
        ))
    );
    let resumed_cid = resumed
        .runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
        .expect("restored agent");
    let current_head = resumed.agent_runtime.agent_registry.agents[&resumed_cid]
        .identity
        .head
        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
    let local_transaction_id = records
        .iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started)
                if matches!(
                    started.trigger,
                    tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
                ) =>
            {
                Some(started.transaction_id.clone())
            }
            _ => None,
        })
        .expect("local legacy claim");
    let successor_id =
        tau_proto::CompactionTransactionId::parse("ct-legacy-explicit-success").expect("id");
    let successor_prompt_id = test_agent_prompt_id("ap-legacy-explicit-success");
    resumed
        .session_runtime
        .agent_store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: agent_id.clone(),
                transaction_id: successor_id.clone(),
                compact_prompt_id: successor_prompt_id.clone(),
                cut: tau_proto::AgentHead::Root,
                resume_through: Some(current_head),
                model: "test/model".into(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: tau_proto::PromptOriginator::User,
                supersedes: Some(local_transaction_id),
                trigger: tau_proto::StandaloneCompactionTrigger::Manual,
            }),
            tau_proto::UnixMicros::now(),
        )
        .expect("append explicit successor");
    resumed
        .session_runtime
        .agent_store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentCompacted(tau_proto::AgentCompacted {
                original_input_tokens: None,
                compaction_output_tokens: None,
                agent_id: agent_id.clone(),
                transaction_id: Some(successor_id),
                cut: Some(tau_proto::AgentHead::Root),
                suffix_end: Some(current_head),
                compact_prompt_id: Some(successor_prompt_id),
                model: Some("test/model".into()),
                operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
                replacement_window: vec![ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content: vec![ContentPart::Text {
                        text: "legacy chain recovered".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                })],
            }),
            tau_proto::UnixMicros::now(),
        )
        .expect("append explicit success");
    let current_head = resumed
        .session_runtime
        .agent_store
        .agent(agent_id.as_str())
        .and_then(tau_core::AgentTree::head)
        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
    let unresolved = resumed
        .session_runtime
        .agent_store
        .agent(agent_id.as_str())
        .expect("restored tree")
        .unresolved_standalone_compaction_failure(&"test/model".into(), current_head);
    assert!(
        unresolved.is_none(),
        "one explicit success must clear the represented chain: {unresolved:?}"
    );
    resumed.shutdown().expect("shutdown");
    drop(resumed);
    wait_for_session_unlock(&state, "s1");

    let mut reopened =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("second resume");
    enable_remote_compaction_for_test_model(&mut reopened);
    reopened.reconcile_pending_context_recoveries(true);
    reopened.drain_publish_idle_dispatches();
    let reopened_records = loaded_agent_events(&reopened, "s1");
    assert_eq!(
        reopened_records
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
        local_starts
    );
    assert_eq!(
        reopened_records
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.reason == tau_proto::StandaloneCompactionFailureReason::StaleBranch
            ))
            .count(),
        local_failures
    );
    let reopened_cid = reopened
        .runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
        .expect("reopened agent");
    let reopened_head = reopened.agent_runtime.agent_registry.agents[&reopened_cid]
        .identity
        .head
        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
    assert!(
        reopened
            .session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("reopened tree")
            .unresolved_standalone_compaction_failure(&"test/model".into(), reopened_head)
            .is_none()
    );
    reopened.shutdown().expect("shutdown");
}
