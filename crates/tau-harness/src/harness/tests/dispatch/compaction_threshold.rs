//! Focused automatic-compaction threshold scheduling tests.

use super::*;

/// A reserve boundary resolves against the selected model's separate legal
/// input limit and preserves threshold equality for proactive compaction.
#[test]
fn reserve_policy_schedules_at_selected_model_boundary() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    establish_exact_provider_usage(&mut h, &cid, 90);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.context_window = tau_proto::TokenCount::new(120);
    info.max_input_tokens = Some(tau_proto::TokenCount::new(100));
    info.supports_standalone_compaction = true;
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    role.compactions.clear();
    role.compactions.insert(
        "reserve".to_owned(),
        tau_config::settings::CompactionPolicy {
            threshold: path_tau_config_settings::CompactionPolicyThreshold::Reserve(10),
            enable: true,
            when: path_tau_config_settings::ContextPolicyWhen::default(),
        },
    );

    assert!(h.schedule_standalone_auto_compaction_for_activation(&cid, true, None));
    let evidence = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => match started.trigger {
                tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { evidence } => {
                    Some(evidence)
                }
                _ => None,
            },
            _ => None,
        })
        .expect("automatic evidence");
    assert_eq!(evidence.threshold, tau_proto::TokenCount::new(90));
    h.shutdown().expect("shutdown");
}

/// A reserve equal to the selected context window is valid and resolves
/// exactly to zero, which remains absence of proactive scheduling authority.
#[test]
fn reserve_equal_to_context_window_does_not_gain_zero_threshold_authority() {
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
    info.context_window = tau_proto::TokenCount::new(100);
    info.supports_standalone_compaction = true;
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    role.compactions.clear();
    role.compactions.insert(
        "reserve".to_owned(),
        tau_config::settings::CompactionPolicy {
            threshold: path_tau_config_settings::CompactionPolicyThreshold::Reserve(100),
            enable: true,
            when: path_tau_config_settings::ContextPolicyWhen::default(),
        },
    );

    assert!(!h.schedule_standalone_auto_compaction_for_activation(&cid, true, None));
    h.shutdown().expect("shutdown");
}

/// Outer-turn policy authority is frozen when the provider prompt is built, so
/// an in-flight model metadata update cannot reinterpret a reserve boundary.
#[test]
fn reserve_policy_uses_dispatch_time_context_window_after_model_update() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.context_window = tau_proto::TokenCount::new(100);
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    role.compactions.clear();
    role.compactions.insert(
        "reserve".to_owned(),
        tau_config::settings::CompactionPolicy {
            threshold: path_tau_config_settings::CompactionPolicyThreshold::Reserve(10),
            enable: true,
            when: path_tau_config_settings::ContextPolicyWhen {
                at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
                statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
            },
        },
    );
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("finish".to_owned()))
        .expect("dispatch inference");
    let inference = read_nth_prompt_created(&h, 0);
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(tau_proto::AgentWorkStatusPhase::Done, "done".to_owned())
            .expect("status"),
    )
    .expect("report status");
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .context_window = tau_proto::TokenCount::new(200);
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
        .expect("dispatch-time reserve decision");
    assert_eq!(decision.threshold, tau_proto::TokenCount::new(90));
    h.shutdown().expect("shutdown");
}

/// Activation preflight must reject an oversized reserve before a valid sibling
/// can authorize paid standalone provider work.
#[test]
fn invalid_reserve_blocks_activation_before_eligible_sibling_schedules() {
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
    info.context_window = tau_proto::TokenCount::new(100);
    info.supports_standalone_compaction = true;
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    role.compactions.clear();
    for (name, threshold) in [
        (
            "invalid",
            path_tau_config_settings::CompactionPolicyThreshold::Reserve(101),
        ),
        (
            "eligible",
            path_tau_config_settings::CompactionPolicyThreshold::Tokens(1),
        ),
    ] {
        role.compactions.insert(
            name.to_owned(),
            tau_config::settings::CompactionPolicy {
                threshold,
                enable: true,
                when: path_tau_config_settings::ContextPolicyWhen::default(),
            },
        );
    }
    let starts_before = event_log_count(&h, |event| {
        matches!(event, Event::AgentStandaloneCompactionStarted(_))
    });

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("next".to_owned()))
        .expect("accept activation");

    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::AgentStandaloneCompactionStarted(_)
        )),
        starts_before
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.kind.as_str() == tau_proto::notice_kind::HARNESS_FAILURE
                && notice.message.contains("compactions.invalid.reserve")
    )));
    h.shutdown().expect("shutdown");
}

/// The legacy fallback reserve is still an explicit role boundary and must not
/// be mislabeled as provider-default authority in durable evidence.
#[test]
fn legacy_reserve_fallback_records_role_threshold_source() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    establish_exact_provider_usage(&mut h, &cid, 90);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.context_window = tau_proto::TokenCount::new(100);
    info.supports_standalone_compaction = true;
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    role.compactions.clear();
    role.compaction = Some(path_tau_config_settings::RoleCompaction::Reserve(10));
    role.inference_compaction = None;

    assert!(h.schedule_standalone_auto_compaction_for_activation(&cid, true, None));
    let evidence = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => match started.trigger {
                tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { evidence } => {
                    Some(evidence)
                }
                _ => None,
            },
            _ => None,
        })
        .expect("automatic evidence");
    assert_eq!(
        evidence.threshold_source,
        tau_proto::CompactionThresholdSource::RoleThreshold
    );
    h.shutdown().expect("shutdown");
}

/// Cold-restored usage must validate reserves against the separate legal input
/// limit before a valid sibling can schedule standalone work.
#[test]
fn invalid_reserve_blocks_cold_activation_before_eligible_sibling() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        let cid = ensure_test_user_agent(&mut h);
        establish_exact_provider_usage(&mut h, &cid, 100);
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("cold resume");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.context_window = tau_proto::TokenCount::new(200);
    info.max_input_tokens = Some(tau_proto::TokenCount::new(100));
    info.supports_standalone_compaction = true;
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    role.compactions.clear();
    for (name, threshold) in [
        (
            "invalid",
            path_tau_config_settings::CompactionPolicyThreshold::Reserve(101),
        ),
        (
            "eligible",
            path_tau_config_settings::CompactionPolicyThreshold::Tokens(1),
        ),
    ] {
        role.compactions.insert(
            name.to_owned(),
            tau_config::settings::CompactionPolicy {
                threshold,
                enable: true,
                when: path_tau_config_settings::ContextPolicyWhen::default(),
            },
        );
    }
    let starts_before = event_log_count(&h, |event| {
        matches!(event, Event::AgentStandaloneCompactionStarted(_))
    });

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("resume".to_owned()))
        .expect("accept activation");

    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::AgentStandaloneCompactionStarted(_)
        )),
        starts_before
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.message.contains("compactions.invalid.reserve")
    )));
    h.shutdown().expect("shutdown resumed");
}

/// Disabled reserve policies do not participate in validation or prompt-time
/// freezing and therefore cannot panic materialization.
#[test]
fn disabled_oversized_reserve_is_ignored_during_prompt_materialization() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.context_window = tau_proto::TokenCount::new(100);
    info.supports_standalone_compaction = true;
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    role.compactions.clear();
    role.compactions.insert(
        "disabled-invalid".to_owned(),
        tau_config::settings::CompactionPolicy {
            threshold: path_tau_config_settings::CompactionPolicyThreshold::Reserve(101),
            enable: false,
            when: path_tau_config_settings::ContextPolicyWhen::default(),
        },
    );

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("continue".to_owned()))
        .expect("dispatch with disabled policy");

    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.operation == tau_proto::PromptOperation::Inference
    )));
    h.shutdown().expect("shutdown");
}

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
                when: path_tau_config_settings::ContextPolicyWhen {
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
                when: path_tau_config_settings::ContextPolicyWhen {
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
