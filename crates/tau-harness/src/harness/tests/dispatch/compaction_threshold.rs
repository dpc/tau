//! Focused automatic-compaction threshold scheduling tests.

use super::*;

/// A zero provider threshold is absence of useful scheduling authority, not an
/// instruction to create an automatic transaction for every observed prompt.
#[test]
fn zero_provider_threshold_does_not_schedule_automatic_compaction() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    establish_exact_provider_usage(&mut h, &cid, 1);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::ZERO);

    assert!(!h.schedule_standalone_auto_compaction_for_activation(&cid, true, None));
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );
    h.shutdown().expect("shutdown");
}

/// A zero named policy must not suppress a positive sibling at the
/// before-inference scheduler or appear in its durable threshold evidence.
#[test]
fn zero_named_policy_does_not_suppress_positive_before_inference_sibling() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    establish_exact_provider_usage(&mut h, &cid, 100);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    for (name, threshold) in [("zero", 0), ("positive", 100)] {
        role.compactions.insert(
            name.to_owned(),
            tau_config::settings::CompactionPolicy {
                threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(threshold),
                enable: true,
                when: tau_config::settings::ContextPolicyWhen {
                    at: path_tau_config_settings::ContextPolicyPoint::BeforeInference,
                    statuses: None,
                },
            },
        );
    }

    assert!(h.schedule_standalone_auto_compaction_for_activation(&cid, true, None));
    let starts = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1);
    assert!(matches!(
        &starts[0].trigger,
        tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { evidence }
            if evidence.threshold == tau_proto::TokenCount::new(100)
                && evidence.threshold_source
                    == tau_proto::CompactionThresholdSource::NamedPolicies {
                        names: vec!["positive".to_owned()]
                    }
    ));
    h.shutdown().expect("shutdown");
}

/// A zero outer-turn policy is not durable decision authority and cannot
/// suppress a positive sibling that must claim the completed ordinary turn.
#[test]
fn zero_named_policy_does_not_suppress_positive_outer_turn_sibling() {
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
    for (name, threshold) in [("zero", 0), ("positive", 100)] {
        role.compactions.insert(
            name.to_owned(),
            tau_config::settings::CompactionPolicy {
                threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(threshold),
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
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(tau_proto::AgentWorkStatusPhase::Done, "finished".to_owned())
            .expect("valid status"),
    )
    .expect("report status");
    let mut response =
        provider_text_response(&inference.agent_prompt_id, inference.agent_id, "done");
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 100,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 1,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(response)
        .expect("finish inference");

    let decision = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::ProviderResponseFinished(response) => response.automatic_compaction_decision,
            _ => None,
        })
        .expect("positive policy decision");
    assert_eq!(decision.threshold, tau_proto::TokenCount::new(100));
    assert_eq!(
        decision
            .evidence
            .expect("positive provider evidence")
            .threshold_source,
        tau_proto::CompactionThresholdSource::NamedPolicies {
            names: vec!["positive".to_owned()],
        }
    );
    h.shutdown().expect("shutdown");
}
