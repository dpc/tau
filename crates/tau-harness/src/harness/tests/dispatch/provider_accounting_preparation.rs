//! Read-only terminal enrichment and effectful attachment equivalence.

use tau_proto::ProviderTokenUsage;

use super::*;
use crate::harness::context_limit_telemetry::PromptContextLimitSnapshot;
use crate::harness::standalone_execution_accounting_state::StandaloneExecutionAccountingOwner;

/// Preparation must project the same saturated counters as attachment without
/// charging the session or consuming the model. Standalone facts, not their
/// terminal preparation, remain responsible for committing their usage.
#[test]
fn provider_usage_preparation_is_read_only_and_matches_attachment() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let model = tau_proto::ModelId::from("provider/captured");
    let other_model = tau_proto::ModelId::from("provider/other");
    h.config.selected_model = Some(other_model.clone());
    let prompt = test_agent_prompt_id("usage-preparation");
    let mut response = provider_text_response(&prompt, crate::parse_agent_id("main"), "ok");
    response.usage = Some(ProviderTokenUsage {
        model: Some(other_model.clone()),
        prompt_cache_read_ceiling_tokens: Some(8),
        cache: Some(Box::new(tau_proto::ProviderCacheUsage {
            read_tokens: Some(9),
            write_tokens: Some(4),
            ..Default::default()
        })),
        ..Default::default()
    });
    let reported_usage = response.usage.clone();
    let stats = &mut h.session_runtime.current_session_state.token_usage;
    stats.start_request(&model);
    stats.add_sent(&model, u64::MAX - 5, 10);
    stats.add_received(&model, u64::MAX - 2);
    stats.start_request(&other_model);
    stats.add_sent(&other_model, 3, 1);
    let before = stats.clone();
    let events_before = event_log_events(&h).len();

    for update_live_totals in [false, true] {
        h.prompt_coordination
            .prompt_runtime
            .models
            .insert(prompt.clone(), model.clone());
        let prepared = h
            .prepare_finished_response_usage(
                &response,
                Some(20),
                Some(2),
                Some(7),
                update_live_totals,
            )
            .expect("known usage");
        assert_eq!(
            h.prepare_finished_response_usage(
                &response,
                Some(20),
                Some(2),
                Some(7),
                update_live_totals
            ),
            Some(prepared.clone()),
            "repeated preparation must not consume or account anything"
        );
        assert_eq!(response.usage, reported_usage);
        assert_eq!(h.session_runtime.current_session_state.token_usage, before);
        assert_eq!(h.prompt_coordination.prompt_runtime.models[&prompt], model);
        assert_eq!(event_log_events(&h).len(), events_before);
        assert_eq!(prepared.model.as_ref(), Some(&model));
        assert_eq!(prepared.prompt_sent_tokens, 20);
        assert_eq!(prepared.prompt_cached_tokens, 9, "nested read count wins");
        assert_eq!(prepared.response_received_tokens, 7);
        assert_eq!(prepared.prompt_cache_read_ceiling_tokens, None);
        assert_eq!(
            prepared.cache.as_ref().expect("cache").write_tokens,
            Some(4)
        );
        assert_eq!(
            prepared.stats.by_model[&other_model],
            before.by_model[&other_model]
        );
        if update_live_totals {
            assert_eq!(prepared.stats.total.sent_tokens, u64::MAX);
            assert_eq!(prepared.stats.total.received_tokens, u64::MAX);
            assert_eq!(prepared.stats.total.cached_tokens, 20);
            assert_eq!(prepared.stats.by_model[&model].cached_tokens, 19);
            assert_eq!(prepared.stats.total.requests, 2);
        } else {
            assert_eq!(prepared.stats, before);
        }
        let mut attached = response.clone();
        h.attach_finished_response_usage(
            &mut attached,
            Some(20),
            Some(2),
            Some(7),
            update_live_totals,
        );
        assert!(
            !h.prompt_coordination
                .prompt_runtime
                .models
                .contains_key(&prompt)
        );
        assert_eq!(attached.usage.as_ref(), Some(&prepared));
        assert_eq!(
            h.session_runtime.current_session_state.token_usage,
            prepared.stats
        );
        assert_eq!(event_log_events(&h).len(), events_before);
    }
    h.shutdown().expect("shutdown");
}

/// Missing observations stay unknown, present zeros stay known, and missing
/// model ownership preserves the existing attachment no-op rather than guessing
/// a model from current selection or provider metadata.
#[test]
fn provider_usage_preparation_preserves_absence_zero_and_missing_model() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let model = tau_proto::ModelId::from("provider/captured");
    let prompt = test_agent_prompt_id("usage-presence");
    let mut response = provider_text_response(&prompt, crate::parse_agent_id("main"), "ok");
    h.prompt_coordination
        .prompt_runtime
        .models
        .insert(prompt.clone(), model.clone());
    assert!(
        h.prepare_finished_response_usage(&response, None, None, None, true)
            .is_none()
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .models
            .contains_key(&prompt)
    );
    h.attach_finished_response_usage(&mut response, None, None, None, true);
    assert!(response.usage.is_none());
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .models
            .contains_key(&prompt)
    );
    assert!(
        h.session_runtime
            .current_session_state
            .token_usage
            .by_model
            .is_empty()
    );

    h.prompt_coordination
        .prompt_runtime
        .models
        .insert(prompt.clone(), model.clone());
    response.usage = Some(ProviderTokenUsage {
        prompt_cache_read_ceiling_tokens: Some(0),
        cache: Some(Box::default()),
        ..Default::default()
    });
    let prepared = h
        .prepare_finished_response_usage(&response, None, Some(0), None, true)
        .expect("zero is observed");
    assert_eq!(prepared.model, Some(model));
    assert_eq!(prepared.prompt_cache_read_ceiling_tokens, Some(0));
    assert_eq!(prepared.cache.as_ref().expect("cache").read_tokens, Some(0));
    assert_eq!(prepared.stats.total, tau_proto::TokenUsageCounts::default());
    h.attach_finished_response_usage(&mut response, None, Some(0), None, true);
    assert_eq!(response.usage.as_ref(), Some(&prepared));

    let before = h.session_runtime.current_session_state.token_usage.clone();
    assert!(
        h.prepare_finished_response_usage(&response, Some(12), None, None, true)
            .is_none()
    );
    h.attach_finished_response_usage(&mut response, Some(12), None, None, true);
    assert_eq!(response.usage.as_ref(), Some(&prepared));
    assert_eq!(h.session_runtime.current_session_state.token_usage, before);
    h.shutdown().expect("shutdown");
}

/// Pricing preview must retain dispatch/standalone snapshots and not charge or
/// publish. Application still chooses dispatch before standalone before
/// fallback, and only the ordinary path updates live cost.
#[test]
fn provider_cost_preparation_preserves_authority_and_defers_effects() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let prompt = test_agent_prompt_id("cost-preparation");
    let mut response = provider_text_response(&prompt, agent_id.clone(), "ok");
    response.usage = Some(ProviderTokenUsage {
        prompt_sent_tokens: 1_000_000,
        ..Default::default()
    });
    let dispatch_rates = tau_proto::EstimatedApiCostRates {
        uncached_input: tau_proto::EstimatedUsdPerMillion::from_micro_usd(1_000_000),
        ..tau_proto::ESTIMATED_API_COST_FALLBACK
    };
    let standalone_rates = tau_proto::EstimatedApiCostRates {
        uncached_input: tau_proto::EstimatedUsdPerMillion::from_micro_usd(2_000_000),
        ..tau_proto::ESTIMATED_API_COST_FALLBACK
    };
    h.prompt_coordination
        .prompt_runtime
        .estimated_cost_rates
        .insert(prompt.clone(), dispatch_rates);
    h.prompt_coordination.standalone_accounting.owners.insert(
        prompt.clone(),
        StandaloneExecutionAccountingOwner {
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id,
            cid: cid.clone(),
            transaction_id: tau_proto::CompactionTransactionId::parse("cost-transaction")
                .expect("transaction"),
            model: "provider/captured".into(),
            estimated_cost_rates: standalone_rates,
        },
    );
    let before_cost = h
        .agent_stats_snapshot(&cid)
        .expect("stats")
        .estimated_api_cost;
    let events_before = event_log_events(&h).len();
    let prepared = h
        .prepare_finished_response_estimated_cost(&response)
        .expect("known cost");
    assert_eq!(prepared.rates, dispatch_rates);
    assert!(!prepared.used_fallback);
    assert_eq!(prepared.increment.as_picodollars(), 1_000_000_000_000);
    assert_eq!(
        h.prompt_coordination.prompt_runtime.estimated_cost_rates[&prompt],
        dispatch_rates
    );
    assert!(
        h.prompt_coordination
            .standalone_accounting
            .owners
            .contains_key(&prompt)
    );
    assert_eq!(
        h.agent_stats_snapshot(&cid)
            .expect("stats")
            .estimated_api_cost,
        before_cost
    );
    assert_eq!(event_log_events(&h).len(), events_before);
    assert!(response.estimated_api_cost_rates.is_none());
    assert!(response.estimated_api_cost_increment.is_none());

    h.add_finished_response_estimated_cost(&cid, &mut response, None, false);
    assert_eq!(response.estimated_api_cost_rates, Some(prepared.rates));
    assert_eq!(
        response.estimated_api_cost_increment,
        Some(prepared.increment)
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .estimated_cost_rates
            .contains_key(&prompt)
    );
    assert_eq!(
        h.agent_stats_snapshot(&cid)
            .expect("stats")
            .estimated_api_cost,
        before_cost
    );
    assert_eq!(event_log_events(&h).len(), events_before);

    let prepared = h
        .prepare_finished_response_estimated_cost(&response)
        .expect("owner cost");
    assert_eq!(prepared.rates, standalone_rates);
    assert!(!prepared.used_fallback);
    assert_eq!(prepared.increment.as_picodollars(), 2_000_000_000_000);
    h.add_finished_response_estimated_cost(&cid, &mut response, None, false);
    assert_eq!(
        response.estimated_api_cost_increment,
        Some(prepared.increment)
    );
    h.prompt_coordination
        .standalone_accounting
        .owners
        .remove(&prompt);

    let prepared = h
        .prepare_finished_response_estimated_cost(&response)
        .expect("fallback cost");
    assert!(prepared.used_fallback);
    assert_eq!(prepared.rates, tau_proto::ESTIMATED_API_COST_FALLBACK);
    assert_eq!(prepared.increment.as_picodollars(), 5_000_000_000_000);
    assert_eq!(event_log_events(&h).len(), events_before);
    h.add_finished_response_estimated_cost(&cid, &mut response, None, true);
    assert_eq!(
        response.estimated_api_cost_increment,
        Some(prepared.increment)
    );
    assert_eq!(
        h.agent_stats_snapshot(&cid)
            .expect("stats")
            .estimated_api_cost,
        prepared.increment
    );
    assert!(
        event_log_events(&h).len() > events_before,
        "application publishes updated stats"
    );
    h.shutdown().expect("shutdown");
}

/// Unknown usage must clear stale pricing only during application and still
/// consume the captured rates/publish stats; zero usage retains explicit zero
/// cost.
#[test]
fn provider_cost_preparation_distinguishes_unknown_from_zero() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let prompt = test_agent_prompt_id("unknown-cost");
    let mut response = provider_text_response(&prompt, crate::parse_agent_id("main"), "ok");
    response.estimated_api_cost_rates = Some(tau_proto::ESTIMATED_API_COST_FALLBACK);
    response.estimated_api_cost_increment = Some(tau_proto::EstimatedApiCost::from_picodollars(42));
    h.prompt_coordination
        .prompt_runtime
        .estimated_cost_rates
        .insert(prompt.clone(), tau_proto::ESTIMATED_API_COST_FALLBACK);
    let events_before = event_log_events(&h).len();
    assert!(
        h.prepare_finished_response_estimated_cost(&response)
            .is_none()
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .estimated_cost_rates
            .contains_key(&prompt)
    );
    assert!(response.estimated_api_cost_increment.is_some());
    assert_eq!(event_log_events(&h).len(), events_before);
    h.add_finished_response_estimated_cost(&cid, &mut response, None, false);
    assert!(response.estimated_api_cost_rates.is_none());
    assert!(response.estimated_api_cost_increment.is_none());
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .estimated_cost_rates
            .contains_key(&prompt)
    );
    assert!(event_log_events(&h).len() > events_before);
    response.usage = Some(ProviderTokenUsage::default());
    let prepared = h
        .prepare_finished_response_estimated_cost(&response)
        .expect("known zero");
    assert_eq!(prepared.increment, tau_proto::EstimatedApiCost::default());
    h.add_finished_response_estimated_cost(&cid, &mut response, None, false);
    assert_eq!(
        response.estimated_api_cost_increment,
        Some(prepared.increment)
    );
    h.shutdown().expect("shutdown");
}

/// Telemetry preview must leave the captured dispatch evidence untouched; the
/// existing attachment path consumes it even for a non-context failure.
#[test]
fn provider_context_preparation_preserves_snapshot_until_attachment() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let prompt = test_agent_prompt_id("context-preparation");
    let model = tau_proto::ModelId::from("provider/captured");
    let mut response = provider_text_response(&prompt, crate::parse_agent_id("main"), "ok");
    response.failure_kind = Some(tau_proto::ProviderFailureKind::ContextWindowExceeded);
    response.usage = Some(ProviderTokenUsage {
        prompt_sent_tokens: 90,
        ..Default::default()
    });
    h.prompt_coordination.prompt_runtime.context_limits.insert(
        prompt.clone(),
        PromptContextLimitSnapshot {
            model: model.clone(),
            operation: tau_proto::PromptOperation::Inference,
            transcript_delta_bytes: Some(tau_proto::ByteCount::new(17)),
            advertised_context_window: Some(tau_proto::TokenCount::new(100)),
            compaction_threshold: Some(tau_proto::TokenCount::new(80)),
            compaction_policy: tau_proto::ContextLimitCompactionPolicy::Threshold,
        },
    );
    let events_before = event_log_events(&h).len();
    let prepared = h
        .prepare_finished_response_context_limit(&response)
        .expect("context evidence");
    assert_eq!(prepared.model, model);
    assert_eq!(prepared.operation, tau_proto::PromptOperation::Inference);
    assert_eq!(
        prepared.transcript_delta_bytes,
        Some(tau_proto::ByteCount::new(17))
    );
    assert_eq!(
        prepared.advertised_context_window,
        Some(tau_proto::TokenCount::new(100))
    );
    assert_eq!(
        prepared.compaction_threshold,
        Some(tau_proto::TokenCount::new(80))
    );
    assert_eq!(
        prepared.compaction_policy,
        tau_proto::ContextLimitCompactionPolicy::Threshold
    );
    assert_eq!(
        prepared.provider_input_tokens,
        Some(tau_proto::TokenCount::new(90))
    );
    assert_eq!(
        prepared.observation,
        tau_proto::ContextLimitObservation::RejectedBelowAdvertisedLimit
    );
    assert_eq!(prepared.action, tau_proto::ContextLimitAction::Terminal);
    assert!(!prepared.recovery_eligible);
    assert_eq!(
        h.prepare_finished_response_context_limit(&response),
        Some(prepared.clone())
    );
    assert!(response.context_limit_telemetry.is_none());
    assert_eq!(event_log_events(&h).len(), events_before);
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .context_limits
            .contains_key(&prompt)
    );
    h.attach_context_limit_telemetry(&mut response);
    assert_eq!(response.context_limit_telemetry, Some(prepared));
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .context_limits
            .contains_key(&prompt)
    );
    assert!(
        h.prepare_finished_response_context_limit(&response)
            .is_none()
    );
    assert_eq!(event_log_events(&h).len(), events_before);

    h.prompt_coordination.prompt_runtime.context_limits.insert(
        prompt.clone(),
        PromptContextLimitSnapshot {
            model,
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            transcript_delta_bytes: None,
            advertised_context_window: None,
            compaction_threshold: None,
            compaction_policy: tau_proto::ContextLimitCompactionPolicy::Disabled,
        },
    );
    response.usage = None;
    let unknown = h
        .prepare_finished_response_context_limit(&response)
        .expect("unknown input");
    assert_eq!(unknown.provider_input_tokens, None);
    assert_eq!(
        unknown.observation,
        tau_proto::ContextLimitObservation::InsufficientEvidence
    );
    response.failure_kind = None;
    let prior = response.context_limit_telemetry.clone();
    assert!(
        h.prepare_finished_response_context_limit(&response)
            .is_none()
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .context_limits
            .contains_key(&prompt)
    );
    h.attach_context_limit_telemetry(&mut response);
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .context_limits
            .contains_key(&prompt)
    );
    assert_eq!(
        response.context_limit_telemetry, prior,
        "no fresh telemetry preserves prior value"
    );
    h.shutdown().expect("shutdown");
}
