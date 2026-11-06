//! Byte growth may stop genuine token-policy recurrence, never create token
//! need.

use super::*;

/// Resume one measured replacement, optionally after a complete tool round.
fn measured_continuation(
    h: &mut Harness,
    cid: &AgentId,
    after: Option<u64>,
    after_tool: bool,
) -> tau_proto::AgentPromptCreated {
    h.dispatch_prompt_for_agent(cid, PendingPrompt::user("policy input".to_owned()))
        .expect("initial inference");
    let mut index = 0;
    let mut inference = read_nth_prompt_created(h, index);
    if after_tool {
        h.handle_provider_response_finished(provider_tool_response(
            &inference,
            "growth-policy-tool",
            "self_info",
            CborValue::Map(Vec::new()),
        ))
        .expect("complete tool round");
        index += 1;
        inference = read_nth_prompt_created(h, index);
    }
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("genuine capacity need");
    index += 1;
    let compact = read_nth_prompt_created(h, index);
    let mut summary = provider_text_response(
        &compact.agent_prompt_id,
        compact.agent_id.clone(),
        "policy replacement",
    );
    summary.compaction_input_bytes =
        after.map(|after| tau_proto::CompactionInputBytes { before: 100, after });
    h.handle_provider_response_finished(summary)
        .expect("accepted replacement");
    read_nth_prompt_created(h, index + 1)
}

/// Enable standalone recovery without introducing an inferred input baseline.
fn enable_measured_policy(h: &mut Harness) {
    enable_remote_compaction_for_test_model(h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = None;
    info.standalone_compaction_prefix_budget = None;
}

/// Supply actual ordinary provider input usage, independent of compact output
/// accounting and the byte comparison.
fn usage_terminal(prompt: &tau_proto::AgentPromptCreated) -> ProviderResponseFinished {
    let mut response = provider_text_response(
        &prompt.agent_prompt_id,
        prompt.agent_id.clone(),
        "policy done",
    );
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 250,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 1,
        stats: Default::default(),
    });
    response
}

/// Initial and post-tool settled terminals retain exactly one canonical
/// terminal and outer finish. Strict growth suppresses real threshold
/// recurrence; equal or unknown measurements preserve scheduling and cold
/// validation.
#[test]
fn compaction_growth_policy_terminal_uses_real_usage_and_finishes_once() {
    for after_tool in [false, true] {
        for after in [None, Some(100), Some(101)] {
            let td = TempDir::new().expect("tempdir");
            let mut h = quiet_provider_harness(td.path()).expect("harness");
            enable_measured_policy(&mut h);
            let cid = ensure_test_user_agent(&mut h);
            h.config
                .available_roles
                .get_mut(&h.config.selected_role)
                .expect("role")
                .compactions
                .insert(
                    "growth-policy".to_owned(),
                    tau_config::settings::CompactionPolicy {
                        threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(100),
                        enable: true,
                        when: tau_config::settings::ContextPolicyWhen {
                            at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
                            statuses: None,
                        },
                    },
                );
            let resumed = measured_continuation(&mut h, &cid, after, after_tool);
            let before = h
                .session_runtime
                .agent_store
                .agent(resumed.agent_id.as_str())
                .expect("tree")
                .clone();
            h.handle_provider_response_finished(usage_terminal(&resumed))
                .expect("terminal");
            let records = h
                .session_runtime
                .agent_store
                .snapshot_agent_events_for_test(resumed.agent_id.as_str())
                .expect("records");
            let terminal = records
                .iter()
                .find_map(|record| match &record.event {
                    Event::ProviderResponseFinished(response)
                        if response.agent_prompt_id == resumed.agent_prompt_id =>
                    {
                        Some(response.clone())
                    }
                    _ => None,
                })
                .expect("canonical terminal");
            let expected = after != Some(101);
            assert_eq!(terminal.automatic_compaction_decision.is_some(), expected);
            assert_eq!(records.iter().filter(|record| matches!(&record.event,
                Event::ProviderResponseFinished(response) if response.agent_prompt_id == resumed.agent_prompt_id
            )).count(), 1);
            let finishes = records
                .iter()
                .filter_map(|record| match &record.event {
                    Event::AgentOuterTurnFinished(finish) => Some(finish),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(finishes.len(), 1);
            assert_eq!(records.iter().filter(|record| matches!(&record.event,
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(started.trigger, tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. })
            )).count(), usize::from(expected));
            assert!(
                h.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .is_empty()
            );

            // This otherwise valid decision probes core's guard independently
            // of the live scheduler, rather than accepting any
            // rejection as proof.
            let mut probe = terminal.clone();
            probe.automatic_compaction_decision = Some(tau_proto::AutomaticCompactionDecision {
                transaction_id: tau_proto::CompactionTransactionId::parse("ct-growth-probe")
                    .expect("id"),
                outer_turn_id: finishes[0].outer_turn_id.clone(),
                model: resumed.model.clone(),
                threshold: tau_proto::TokenCount::new(100),
                evidence: Some(policy_evidence(&resumed)),
            });
            assert_eq!(
                before
                    .validate_event(&Event::ProviderResponseFinished(probe))
                    .is_ok(),
                expected
            );
            let cold = tau_core::AgentTree::from_events(resumed.agent_id.clone(), &records);
            assert_eq!(
                cold.preceding_compaction_grew(&resumed.agent_prompt_id),
                !expected
            );
            h.shutdown().expect("shutdown");
        }
    }
}

/// Exact successful usage remains the only proactive evidence.
fn policy_evidence(
    prompt: &tau_proto::AgentPromptCreated,
) -> tau_proto::ProactiveCompactionEvidence {
    tau_proto::ProactiveCompactionEvidence {
        provider_prompt_id: prompt.agent_prompt_id.clone(),
        provider_input_tokens: tau_proto::TokenCount::new(250),
        threshold: tau_proto::TokenCount::new(100),
        threshold_source: tau_proto::CompactionThresholdSource::NamedPolicies {
            names: vec!["growth-policy".to_owned()],
        },
    }
}

/// Before-inference policy uses the same grown comparison and real usage after
/// cold reconstruction. Unknown/equal comparisons still admit exactly one
/// start.
#[test]
fn compaction_growth_policy_before_inference_matches_cold_usage_authority() {
    for after in [None, Some(100), Some(101)] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        enable_measured_policy(&mut h);
        let cid = ensure_test_user_agent(&mut h);
        let resumed = measured_continuation(&mut h, &cid, after, false);
        h.handle_provider_response_finished(usage_terminal(&resumed))
            .expect("terminal");
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(resumed.agent_id.as_str())
            .expect("records");
        let evidence = policy_evidence(&resumed);
        let expected = after != Some(101);
        let live = h
            .session_runtime
            .agent_store
            .agent(resumed.agent_id.as_str())
            .expect("tree");
        assert_eq!(
            live.historical_proactive_evidence_is_valid(&evidence, &resumed.model),
            expected
        );
        let cold = tau_core::AgentTree::from_events(resumed.agent_id.clone(), &records);
        assert_eq!(
            cold.historical_proactive_evidence_is_valid(&evidence, &resumed.model),
            expected
        );
        h.shutdown().expect("shutdown");
        drop(h);
        wait_for_session_unlock(td.path(), "s1");
        let mut h = quiet_provider_harness_with_start_reason(
            td.path(),
            tau_proto::SessionStartReason::Resume,
        )
        .expect("cold harness");
        enable_measured_policy(&mut h);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("model")
            .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(100));
        let cid = h
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(resumed.agent_id.as_str())
            .expect("restored route")
            .clone();
        assert_eq!(
            h.schedule_standalone_auto_compaction_for_activation(&cid, true),
            expected
        );
        assert!(!h.schedule_standalone_auto_compaction_for_activation(&cid, true));
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
                .count(),
            usize::from(expected)
        );
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .is_empty()
        );
        h.shutdown().expect("shutdown");
    }
}
