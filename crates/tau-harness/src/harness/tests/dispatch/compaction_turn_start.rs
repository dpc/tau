//! Lazy new-outer-turn policy boundaries and independent mid-turn safety.

use tau_config::settings::{
    CompactionPolicy, CompactionPolicyThreshold, ContextPolicyPoint, ContextPolicyWhen,
};

use super::*;

/// Install only the requested checkpoint, keeping test thresholds independent
/// of provider defaults.
fn install_policy(
    h: &mut Harness,
    point: ContextPolicyPoint,
    statuses: Option<Vec<tau_proto::AgentWorkStatusPhase>>,
) {
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("role");
    role.compactions.clear();
    role.compactions.insert(
        "boundary".to_owned(),
        CompactionPolicy {
            threshold: CompactionPolicyThreshold::Tokens(100),
            enable: true,
            when: ContextPolicyWhen {
                at: point,
                statuses,
            },
        },
    );
}

/// Count committed protected starts rather than provider calls.
fn starts(h: &Harness) -> usize {
    event_log_count(h, |event| {
        matches!(event, Event::AgentStandaloneCompactionStarted(_))
    })
}

/// Initial and post-tool normal/canceled terminals must finish exactly once
/// without eager compaction; only the next admitted turn may compact.
#[test]
fn outer_turn_starting_is_lazy_after_normal_and_canceled_terminals() {
    for post_tool in [false, true] {
        for canceled in [false, true] {
            let td = TempDir::new().expect("tempdir");
            let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
            enable_remote_compaction_for_test_model(&mut h);
            let cid = ensure_test_user_agent(&mut h);
            install_policy(&mut h, ContextPolicyPoint::OuterTurnStarting, None);
            h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("first task".to_owned()))
                .expect("dispatch");
            let initial = read_nth_prompt_created(&h, 0);
            let terminal = if post_tool {
                let mut response = provider_tool_response(
                    &initial,
                    "start-policy-tool",
                    "self_info",
                    CborValue::Map(Vec::new()),
                );
                response.usage = Some(tau_proto::ProviderTokenUsage {
                    prompt_sent_tokens: 200,
                    ..Default::default()
                });
                h.handle_provider_response_finished(response)
                    .expect("tool round");
                assert_eq!(starts(&h), 0, "tool continuation is not a new turn");
                read_nth_prompt_created(&h, 1)
            } else {
                initial.clone()
            };
            if canceled {
                h.finalize_canceled_in_flight_prompt(&cid);
            } else {
                let mut response = provider_text_response(
                    &terminal.agent_prompt_id,
                    terminal.agent_id.clone(),
                    "finished",
                );
                response.usage = Some(tau_proto::ProviderTokenUsage {
                    prompt_sent_tokens: 200,
                    ..Default::default()
                });
                h.handle_provider_response_finished(response)
                    .expect("terminal");
            }
            assert_eq!(starts(&h), 0, "finish alone never compacts");
            assert_eq!(
                event_log_count(&h, |event| matches!(event,
                    Event::ProviderResponseFinished(response) if response.agent_prompt_id == terminal.agent_prompt_id
                ) || matches!(event,
                    Event::AgentPromptTerminated(terminated) if terminated.agent_prompt_id == terminal.agent_prompt_id
                )),
                1
            );
            let turn_id = tau_proto::AgentOuterTurnId::for_prompt(&initial.agent_prompt_id);
            assert_eq!(
                event_log_count(&h, |event| matches!(event,
                    Event::AgentOuterTurnFinished(finished) if finished.outer_turn_id == turn_id
                )),
                1
            );
            assert!(
                h.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .is_empty()
            );
            h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("next task".to_owned()))
                .expect("next turn");
            // An initial cancellation has no accepted provider usage authority.
            let expected = usize::from(!canceled || post_tool);
            assert_eq!(starts(&h), expected);
            h.drain_publish_idle_dispatches();
            h.try_advance_queue();
            assert_eq!(
                starts(&h),
                expected,
                "repeated drains cannot duplicate a start"
            );
            h.shutdown().expect("shutdown");
        }
    }
}

/// A prior done report survives selection, and overlapping safety/start rules
/// must create one transaction before one new outer turn.
#[test]
fn outer_turn_starting_preserves_prior_done_and_coalesces_safety() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    establish_exact_provider_usage(&mut h, &cid, 200);
    h.install_internal_tool_handlers(vec![std::sync::Arc::new(RejectingStatusTool)]);
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Done,
            "previous task complete".to_owned(),
        )
        .expect("report"),
    )
    .expect("status");
    install_policy(
        &mut h,
        ContextPolicyPoint::OuterTurnStarting,
        Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
    );
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("role");
    role.compactions.insert(
        "safety".to_owned(),
        CompactionPolicy {
            threshold: CompactionPolicyThreshold::Tokens(150),
            enable: true,
            when: ContextPolicyWhen::default(),
        },
    );
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("new task".to_owned()))
        .expect("dispatch");
    assert_eq!(starts(&h), 1);
    let start = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(start) => Some(start),
            _ => None,
        })
        .expect("start");
    let tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { evidence } =
        start.trigger
    else {
        panic!("threshold evidence");
    };
    assert_eq!(evidence.threshold, tau_proto::TokenCount::new(100));
    assert_eq!(
        evidence.threshold_source,
        tau_proto::CompactionThresholdSource::NamedPolicies {
            names: vec!["boundary".to_owned(), "safety".to_owned()]
        }
    );
    let compact = read_nth_prompt_created(&h, 1);
    h.handle_provider_response_finished(provider_text_response(
        &compact.agent_prompt_id,
        compact.agent_id,
        "summary",
    ))
    .expect("compact");
    let ordinary = read_nth_prompt_created(&h, 2);
    assert_eq!(ordinary.operation, tau_proto::PromptOperation::Inference);
    assert_eq!(starts(&h), 1);
    assert_eq!(
        event_log_count(&h, |event| matches!(event, Event::AgentOuterTurnStarted(_))),
        2
    );
    h.shutdown().expect("shutdown");
}

/// The new boundary must not disable independent safety compaction between
/// a tool result and continuation inference.
#[test]
fn outer_turn_starting_keeps_before_inference_safety_midturn() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    install_policy(&mut h, ContextPolicyPoint::OuterTurnStarting, None);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("task".to_owned()))
        .expect("dispatch");
    let initial = read_nth_prompt_created(&h, 0);
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("role")
        .compactions
        .insert(
            "safety".to_owned(),
            CompactionPolicy {
                threshold: CompactionPolicyThreshold::Tokens(150),
                enable: true,
                when: ContextPolicyWhen::default(),
            },
        );
    let mut response = provider_tool_response(
        &initial,
        "safety-tool",
        "self_info",
        CborValue::Map(Vec::new()),
    );
    response.usage = Some(tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 200,
        ..Default::default()
    });
    h.handle_provider_response_finished(response).expect("tool");
    assert_eq!(starts(&h), 1);
    let start = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(start) => Some(start),
            _ => None,
        })
        .expect("start");
    let tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { evidence } =
        start.trigger
    else {
        panic!("threshold evidence");
    };
    assert_eq!(evidence.threshold, tau_proto::TokenCount::new(150));
    assert_eq!(
        evidence.threshold_source,
        tau_proto::CompactionThresholdSource::NamedPolicies {
            names: vec!["safety".to_owned()]
        }
    );
    h.shutdown().expect("shutdown");
}

/// Queued inputs are not a compaction trigger while the previous turn owns
/// inference; their eventual new turn shares one protected start.
#[test]
fn outer_turn_starting_waits_for_queued_work_to_begin_a_new_turn() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    install_policy(&mut h, ContextPolicyPoint::OuterTurnStarting, None);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("first task".to_owned()))
        .expect("dispatch");
    let first = read_nth_prompt_created(&h, 0);
    for text in ["queued one", "queued two"] {
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user(text.to_owned()))
            .expect("queue");
    }
    h.try_advance_queue();
    assert_eq!(starts(&h), 0);
    let mut response = provider_text_response(&first.agent_prompt_id, first.agent_id, "finished");
    response.usage = Some(tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 200,
        ..Default::default()
    });
    h.handle_provider_response_finished(response)
        .expect("finish");
    h.try_advance_queue();
    h.drain_publish_idle_dispatches();
    assert_eq!(starts(&h), 1);
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::AgentOuterTurnFinished(_)
        )),
        1
    );
    let compact = read_nth_prompt_created(&h, 1);
    h.handle_provider_response_finished(provider_text_response(
        &compact.agent_prompt_id,
        compact.agent_id,
        "summary",
    ))
    .expect("compact");
    h.try_advance_queue();
    assert_eq!(starts(&h), 1);
    assert_eq!(
        event_log_count(&h, |event| matches!(event, Event::AgentOuterTurnStarted(_))),
        2
    );
    h.shutdown().expect("shutdown");
}

/// A protected start interrupted by restart must retain its normal recovery
/// suppression, not gain fresh start authority from the absent runtime turn.
#[test]
fn outer_turn_starting_restart_preserves_transaction() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let target;
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        let cid = ensure_test_user_agent(&mut h);
        target = durable_agent_id_for_conversation(&h, &cid);
        establish_exact_provider_usage(&mut h, &cid, 200);
        install_policy(&mut h, ContextPolicyPoint::OuterTurnStarting, None);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("next task".to_owned()))
            .expect("dispatch");
        assert_eq!(starts(&h), 1);
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(target.as_str())
            .expect("records");
        let cold = tau_core::AgentTree::from_events(target.clone(), &records);
        assert_eq!(
            cold.standalone_compaction_recovery(),
            h.session_runtime
                .agent_store
                .agent(target.as_str())
                .expect("tree")
                .standalone_compaction_recovery()
        );
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    install_policy(&mut h, ContextPolicyPoint::OuterTurnStarting, None);
    assert!(!h.schedule_standalone_auto_compaction_for_activation(&cid, true));
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(target.as_str())
        .expect("records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        1
    );
    assert!(matches!(
        h.session_runtime
            .agent_store
            .agent(target.as_str())
            .expect("tree")
            .standalone_compaction_recovery(),
        Some(tau_core::StandaloneCompactionRecovery::Blocked { .. })
    ));
    h.shutdown().expect("shutdown");
}

/// A rejected start append retains admission ownership; repeated scheduler
/// drains must not publish duplicate compactions for the same new turn.
#[test]
fn outer_turn_starting_rejected_start_retries_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    establish_exact_provider_usage(&mut h, &cid, 200);
    install_policy(&mut h, ContextPolicyPoint::OuterTurnStarting, None);
    connect_test_tool(&mut h, "lazy-start-cut");
    h.handle_extension_event(
        "lazy-start-cut",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STANDALONE_COMPACTION_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept");
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("next task".to_owned()))
        .expect("dispatch");
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::AgentStandaloneCompactionStarted(_))
    ));
    h.drain_publish_idle_dispatches();
    assert_eq!(starts(&h), 0);
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "lazy-start-cut",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("reject append");
    h.drain_publish_idle_dispatches();
    h.try_advance_queue();
    if h.runtime_io.publication.pending_intercept.is_some() {
        h.handle_extension_event(
            "lazy-start-cut",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("accept retry");
    }
    h.drain_publish_idle_dispatches();
    assert_eq!(starts(&h), 1);
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}
