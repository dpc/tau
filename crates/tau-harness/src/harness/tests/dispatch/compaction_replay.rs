//! Focused restart coverage for automatic compaction.

use super::*;

/// A cold-restored oversized no-resume boundary must claim a timer activation
/// with one inference checkpoint while a later visible prompt queues behind it.
#[test]
fn no_resume_auto_compaction_replay_checkpoints_timer_before_visible_queue() {
    fn configure_automatic_compaction(
        h: &mut Harness,
        point: path_tau_config_settings::ContextPolicyPoint,
    ) {
        enable_remote_compaction_for_test_model(h);
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(100);
        info.standalone_compaction_prefix_budget = Some(10_000_000);
        let role = h
            .config
            .available_roles
            .get_mut(&h.config.selected_role)
            .expect("selected role");
        role.compactions.clear();
        role.compactions.insert(
            "focused-policy".to_owned(),
            tau_config::settings::CompactionPolicy {
                threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(100),
                enable: true,
                when: tau_config::settings::ContextPolicyWhen {
                    at: point,
                    statuses: None,
                },
            },
        );
    }

    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let agent_id;
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        configure_automatic_compaction(
            &mut h,
            path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
        );
        let cid = ensure_test_user_agent(&mut h);
        agent_id = durable_agent_id_for_conversation(&h, &cid);
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
            crate::WorkStatusReport::new(
                tau_proto::AgentWorkStatusPhase::Done,
                "finished".to_owned(),
            )
            .expect("valid status"),
        )
        .expect("report status");
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
            .expect("finish outer turn");
        let compact = read_nth_prompt_created(&h, 2);
        let checkpoints_before_boundary = event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
            .count();
        h.handle_provider_response_finished(provider_text_response(
            &compact.agent_prompt_id,
            compact.agent_id,
            &format!("oversized replacement {}", "x".repeat(1_000)),
        ))
        .expect("commit no-resume boundary");

        let events = event_log_events(&h);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::ProviderResponseFinished(finished)
                        if finished.agent_prompt_id == continuation.agent_prompt_id
                ))
                .count(),
            1,
            "the canonical outer-turn terminal must commit once"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::AgentOuterTurnFinished(_)))
                .count(),
            1,
            "the outer turn must finish once"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::AgentStandaloneCompactionStarted(started)
                        if started.resume_through.is_none()
                            && matches!(
                                started.trigger,
                                tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. }
                            )
                ))
                .count(),
            1,
            "the finished outer turn must own one no-resume automatic pass"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
                .count(),
            checkpoints_before_boundary,
            "the no-resume boundary must become idle without inventing inference"
        );
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .is_empty(),
            "successful publication must retain no completion"
        );
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");

    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("cold resume");
    configure_automatic_compaction(
        &mut h,
        path_tau_config_settings::ContextPolicyPoint::BeforeInference,
    );
    let cid = ensure_test_user_agent(&mut h);
    let selected = h.agent_runtime.agent_registry.agents[&cid].identity.head;
    let restored_window = h
        .session_runtime
        .agent_store
        .agent(&agent_id)
        .expect("restored agent")
        .active_provider_window(selected);
    assert!(
        restored_window.replacement.is_some() && restored_window.transcript.is_empty(),
        "cold replay must restore the replacement-only logical window"
    );
    let mut timer = PendingPrompt::internal("timer activation".to_owned());
    timer.source = path_crate_agent::PendingPromptSource::Timer;
    h.submit_prompt_to_agent(test_session_id("s1"), &agent_id, timer)
        .expect("accept timer activation");

    let claims = event_log_events(&h)
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::AgentStandaloneCompactionStarted(_)
                    | Event::AgentInferenceDispatchStarted(_)
            )
        })
        .count();
    assert_eq!(
        claims, 1,
        "the restored timer obligation must gain exactly one scheduler claim"
    );
    let timer_inference = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentPromptCreated(prompt)
                if prompt.operation == tau_proto::PromptOperation::Inference =>
            {
                Some(prompt)
            }
            _ => None,
        })
        .expect("replacement-only window must fall through to inference");
    let mut visible = PendingPrompt::user("visible activation".to_owned());
    visible.submission_source = tau_proto::PromptSubmissionSource::HumanUi;
    assert_eq!(
        h.submit_prompt_to_agent(test_session_id("s1"), &agent_id, visible)
            .expect("accept visible activation"),
        PromptSubmission::Queued
    );
    let context = serde_json::to_string(&timer_inference.context).expect("inference context");
    assert_eq!(context.matches("timer activation").count(), 1);
    assert_eq!(context.matches("oversized replacement").count(), 1);
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        1,
        "the timer activation must gain one inference checkpoint"
    );
    assert!(
        h.session_runtime
            .agent_store
            .agent_events(&agent_id)
            .expect("durable agent events")
            .iter()
            .any(|record| matches!(
                &record.event,
                Event::AgentActivationQueued(queued)
                    if queued.kind == tau_proto::ActivationKind::VisibleUser
            ))
    );
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentStandaloneCompactionFailed(_)))
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::DispatchUncertain { .. }
    ));
    let durable = h
        .session_runtime
        .agent_store
        .agent_events(&agent_id)
        .expect("durable agent events");
    assert_eq!(
        durable
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ProviderResponseFinished(finished)
                    if finished.automatic_compaction_decision.is_some()
            ))
            .count(),
        1,
        "cold replay must retain one canonical deciding terminal"
    );
    assert_eq!(
        durable
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentOuterTurnFinished(finished)
                    if finished.automatic_compaction_decision.is_some()
            ))
            .count(),
        1,
        "cold replay must retain one outer-turn finish"
    );
    assert_eq!(
        durable
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        started.trigger,
                        tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. }
                    )
            ))
            .count(),
        1,
        "cold replay must retain one protected automatic start"
    );
    h.shutdown().expect("shutdown");
}
