//! Tests for provider responses behavior.

use std::fs::File;
use std::io::Write;

use super::super::lifecycle::seed_restored_compaction_checkpoint;
use super::*;

/// Ensures provider-owned response stats survive harness validation and are
/// broadcast on `provider.response_updated` without a harness stats projection.
#[test]
fn provider_response_stats_are_public_provider_updates() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    connect_ready_configured_extension(
        &mut h,
        "provider-owner",
        "provider-owner",
        tau_proto::ClientKind::Provider,
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-provider-public-stats");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    let durable_id = durable_agent_id_for_conversation(&h, &cid);
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.provider_runtime
        .pending_prompts
        .insert(spid.clone(), crate::test_connection_id("provider-owner"));
    let ui_frames = connect_test_client(&mut h, "ui-stats-observer", tau_proto::ClientKind::Ui);
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("ui-stats-observer"),
            Vec::new(),
            vec![
                EventSelector::Exact(tau_proto::EventName::PROVIDER_RESPONSE_UPDATED),
                EventSelector::Exact(tau_proto::EventName::AGENT_STATS_UPDATED),
            ],
        )
        .expect("ui stats subscriptions");

    h.handle_extension_event(
        "provider-owner",
        TestProtocolItem::Event(Event::ProviderResponseUpdatedReported(
            ProviderResponseUpdated {
                agent_prompt_id: spid.clone(),
                agent_id: crate::parse_agent_id("forged_agent"),
                deltas: Vec::new(),
                compaction: None,
                status: None,
                response_stats: Some(tau_proto::ProviderResponseStats {
                    current: tau_proto::ProviderResponseStatsSample {
                        response_bytes_received: 4096,
                        elapsed_micros: 2_000_000,
                    },
                    previous: tau_proto::ProviderResponseStatsSample {
                        response_bytes_received: 1024,
                        elapsed_micros: 1_000_000,
                    },
                    first_semantic_output_elapsed_micros: Some(825_000),
                }),
                originator: tau_proto::PromptOriginator::User,
            },
        )),
    )
    .expect("provider stats update");

    let frames = ui_frames.lock().expect("ui frames");
    assert!(
        frames.iter().any(|routed| matches!(
            peel_inner_event(&routed.frame),
            Some(Event::ProviderResponseUpdated(update))
                if update.agent_id == durable_id
                    && update.response_stats.as_ref().is_some_and(|stats|
                        stats.current.response_bytes_received == 4096
                            && stats.previous.response_bytes_received == 1024
                            && stats.first_semantic_output_elapsed_micros == Some(825_000))
        )),
        "provider stats update must broadcast unchanged as provider.response_updated"
    );
    assert!(
        frames.iter().all(|routed| !matches!(
            peel_inner_event(&routed.frame),
            Some(Event::AgentStatsUpdated(_))
        )),
        "provider response stats must not be projected into harness agent stats"
    );

    h.shutdown().expect("shutdown");
}

/// Switching `selected_model` mid-conversation must bust the chain.
/// The prior response was produced by a different model — its
/// stored state on the upstream API is meaningless for the new
/// model, and sending `previous_response_id` would either error or
/// silently mix incompatible reasoning.
#[test]
fn model_switch_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model-a".into());

    h.submit_user_prompt(test_session_id("s1"), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // The selected role resolves to a different model.
    h.config.selected_model = Some("test/model-b".into());

    h.submit_user_prompt(test_session_id("s1"), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );

    h.shutdown().expect("shutdown");
}

/// A turn that didn't yield a `response_id` (Chat Completions
/// backend, an error, etc.) must NOT anchor a chain. The next prompt
/// has to be a full replay — pretending we have a chain we don't
/// would make the upstream API reject the next call.
#[test]
fn missing_response_id_leaves_chain_unset() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    h.submit_user_prompt(test_session_id("s1"), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt(test_session_id("s1"), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );

    h.shutdown().expect("shutdown");
}

/// Provider loss preserves one deferred live occurrence through receipt and
/// terminal append failures. Typed and raw activating ingress both materialize
/// only after the exact intercepted Stale closer commits.
#[test]
fn provider_loss_retries_typed_and_raw_deferred_input_after_append_failures() {
    for raw_fact in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        let mut h = quiet_provider_harness(&state).expect("start");
        let immediate =
            NotificationDeliveryPolicy::from_millis(0, 0, 0).expect("immediate test policy");
        h.config
            .accepted_harness_settings
            .notification_delivery
            .agent_message = immediate;
        h.config
            .accepted_harness_settings
            .notification_delivery
            .external_message = immediate;
        let replacement_model = h
            .provider_runtime
            .model_info
            .values()
            .find(|model| model.id == tau_proto::ModelId::from("test/model"))
            .expect("test model")
            .clone();
        let cid = ensure_test_user_agent(&mut h);
        let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("provider-loss H".to_owned()))
            .expect("dispatch owner");
        let owner = read_nth_prompt_created(&h, 0).agent_prompt_id.clone();
        let provider = h.provider_runtime.pending_prompts[&owner].clone();
        let interceptor = if raw_fact {
            "raw-loss-interceptor"
        } else {
            "typed-loss-interceptor"
        };
        connect_test_tool(&mut h, interceptor);
        h.handle_extension_event(
            interceptor,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_PROMPT_TERMINATED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register terminal interceptor");
        h.handle_disconnect(&provider);
        assert!(matches!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .activation_dispatch,
            crate::agent::ActivationDispatchState::DispatchUncertain { .. }
        ));
        assert!(
            h.runtime_io.publication.pending_intercept.is_none(),
            "provider loss without activating input retains uncertainty without a closer"
        );
        let input = || {
            if raw_fact {
                Event::MessageDelivered(tau_proto::MessageDelivered::new(
                    tau_proto::MessagePublisherId::parse("loss-bridge").expect("publisher"),
                    tau_proto::MessageAgentTarget::new(durable_agent_id.as_str()),
                    tau_proto::MessageFactId::new("loss-raw-input"),
                    tau_proto::MessageParty {
                        stable_id: "external".to_owned(),
                        display_name: None,
                        sender_auth: None,
                    },
                    None,
                    "provider-loss deferred Q",
                ))
            } else {
                Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
                    message_id: tau_proto::AgentMessageId::parse("loss-typed-input")
                        .expect("message id"),
                    sender_id: crate::parse_agent_id("sender"),
                    sender_session_id: None,
                    recipient_id: crate::parse_agent_id(&durable_agent_id),
                    kind: tau_proto::AgentMessageKind::Message,
                    watch_provider_status: None,
                    watch_work_status: None,
                    watch_long_wait: None,
                    watch_lifecycle: None,
                    message: "provider-loss deferred Q".to_owned(),
                })
            }
        };
        reject_next_semantic_admission(&h);
        h.publish_event(
            Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
            input(),
        );
        assert!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .pending_message_wakes
                .is_empty(),
            "failed receipt append creates no runtime obligation"
        );
        assert!(
            h.runtime_io.publication.pending_intercept.is_none(),
            "failed receipt append cannot enqueue Stale"
        );
        h.publish_event(
            Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
            input(),
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .pending_message_wakes
                .len(),
            1,
            "retry creates one sequence-owned wake"
        );
        let source_seq = h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_message_wakes
            .back()
            .expect("retry commits one wake")
            .source
            .durable_event_seq();
        assert!(
            h.session_runtime
                .agent_store
                .agent(&durable_agent_id)
                .and_then(|tree| tree.node_for_durable_event_seq(source_seq))
                .is_none(),
            "receipt remains deferred behind the marked owner"
        );

        assert!(matches!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .activation_dispatch,
            crate::agent::ActivationDispatchState::DispatchUncertain { .. }
        ));
        assert!(
            h.session_runtime
                .agent_store
                .agent(&durable_agent_id)
                .and_then(|tree| tree.node_for_durable_event_seq(source_seq))
                .is_none(),
            "intercepted Stale cannot materialize input"
        );
        reject_next_semantic_admission(&h);
        h.handle_extension_event(
            interceptor,
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("release terminal into append failure");
        assert!(matches!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .activation_dispatch,
            crate::agent::ActivationDispatchState::DispatchUncertain { .. }
        ));
        assert!(
            h.session_runtime
                .agent_store
                .agent(&durable_agent_id)
                .and_then(|tree| tree.node_for_durable_event_seq(source_seq))
                .is_none(),
            "failed closer preserves uncertainty and deferred placement"
        );
        connect_ready_configured_extension(
            &mut h,
            "replacement-provider",
            "replacement-provider",
            tau_proto::ClientKind::Provider,
        );
        h.publish_provider_models_update(
            &crate::test_connection_id("replacement-provider"),
            crate::test_extension_name("replacement-provider"),
            tau_proto::ProviderModelsDeclared {
                models: vec![replacement_model],
            },
        );
        assert!(h.terminalize_uncertain_marked_owner_for_live_activation(&cid));
        h.handle_extension_event(
            interceptor,
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("commit retried terminal");
        h.drain_publish_idle_dispatches();
        h.process_notification_delivery_deadlines_at(Instant::now());
        h.try_advance_queue();
        h.drain_publish_idle_dispatches();
        let node_id = h
            .session_runtime
            .agent_store
            .agent(&durable_agent_id)
            .and_then(|tree| tree.node_for_durable_event_seq(source_seq))
            .expect("one materialized input");
        assert_eq!(
            h.session_runtime
                .agent_store
                .agent(&durable_agent_id)
                .expect("tree")
                .nodes()
                .iter()
                .filter(|node| node.id == node_id)
                .count(),
            1
        );
        assert_eq!(
            h.session_runtime
                .agent_store
                .agent_events(&durable_agent_id)
                .expect("durable records")
                .iter()
                .filter(
                    |record| matches!(&record.event, Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id == owner)
                )
                .count(),
            1
        );
        let records = h
            .session_runtime
            .agent_store
            .agent_events(&durable_agent_id)
            .expect("durable records");
        let terminal_index = records
            .iter()
            .position(|record| {
                matches!(&record.event, Event::AgentPromptTerminated(terminated)
                if terminated.agent_prompt_id == owner)
            })
            .expect("terminal record");
        let successor_checkpoints = records[terminal_index + 1..]
            .iter()
            .filter_map(|record| match &record.event {
                Event::AgentInferenceDispatchStarted(checkpoint) => Some(checkpoint),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(successor_checkpoints.len(), 1);
        assert_eq!(
            successor_checkpoints[0].through,
            tau_proto::AgentHead::Node(node_id)
        );
        assert_eq!(
            successor_checkpoints[0].activation_cut,
            Some(
                h.session_runtime
                    .agent_store
                    .agent(&durable_agent_id)
                    .and_then(|tree| tree.node(node_id))
                    .and_then(|node| node.parent_id)
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node)
            )
        );
        let successors = event_log_events(&h)
            .into_iter()
            .filter_map(|event| match event {
                Event::AgentPromptCreated(prompt) if prompt.agent_prompt_id != owner => {
                    Some(prompt)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(successors.len(), 1);
        assert_eq!(
            serde_json::to_string(&successors[0].context)
                .expect("successor context")
                .matches("provider-loss deferred Q")
                .count(),
            1
        );
        assert!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .pending_message_wakes
                .is_empty()
        );
        h.shutdown().expect("shutdown");
    }
}

/// Authenticated HumanUI prompts queue before one exact retained Stale,
/// coalesce while interception is parked, and dispatch under a fresh prompt id
/// after an admission-rejected terminal retries without another input.
#[test]
fn provider_loss_human_ui_supersession_coalesces_and_retries_exact_stale() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let replacement_model = h
        .provider_runtime
        .model_info
        .values()
        .find(|model| model.id == tau_proto::ModelId::from("test/model"))
        .expect("test model")
        .clone();
    let cid = ensure_test_user_agent(&mut h);
    let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("old owner".to_owned()))
        .expect("dispatch owner");
    let old_prompt_id = read_nth_prompt_created(&h, 0).agent_prompt_id.clone();
    let provider = h.provider_runtime.pending_prompts[&old_prompt_id].clone();
    h.handle_disconnect(&provider);
    connect_ready_configured_extension(
        &mut h,
        "replacement-provider",
        "replacement-provider",
        tau_proto::ClientKind::Provider,
    );
    h.publish_provider_models_update(
        &crate::test_connection_id("replacement-provider"),
        crate::test_extension_name("replacement-provider"),
        tau_proto::ProviderModelsDeclared {
            models: vec![replacement_model],
        },
    );
    connect_test_tool(&mut h, "stale-interceptor");
    h.handle_extension_event(
        "stale-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_TERMINATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    for text in [
        "first replacement",
        "second replacement",
        "third replacement",
    ] {
        submit_authenticated_ui_prompt(
            &mut h,
            crate::parse_agent_id(&durable_agent_id),
            text,
            tau_proto::PromptMessageClass::User,
        )
        .expect("submit authenticated UI prompt");
    }
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .len(),
        3,
        "every accepted prompt remains FIFO-owned"
    );
    assert_eq!(
        h.prompt_coordination
            .prompt_runtime
            .pending_uncertain_supersessions
            .len(),
        1,
        "later prompts coalesce behind one exact Stale owner"
    );
    let replacement = Event::AgentPromptTerminated(tau_proto::AgentPromptTerminated {
        automatic_compaction_decision: None,
        agent_id: durable_agent_id.clone(),
        agent_prompt_id: test_agent_prompt_id("forged-replacement"),
        reason: tau_proto::AgentPromptTerminationReason::Canceled,
        originator: tau_proto::PromptOriginator::User,
    });
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "stale-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(replacement))),
        })),
    )
    .expect("reject exact Stale admission");
    assert!(matches!(
        h.prompt_coordination
            .prompt_runtime
            .pending_uncertain_supersessions[&cid]
            .phase,
        super::super::super::prompt_runtime_state::UncertainSupersessionPhase::RetainedRetry
    ));

    let mut served_clients = 0;
    let mut exit_on_disconnect = false;
    let mut ever_attached = false;
    h.session_runtime
        .persistence_owner
        .as_ref()
        .expect("persistence owner")
        .signal_capacity_ready_for_test();
    h.handle_runtime_event(
        HarnessEvent::Command(HarnessCommand::SemanticPersistenceProgress),
        &mut served_clients,
        &mut exit_on_disconnect,
        &mut ever_attached,
    )
    .expect("retry retained Stale from capacity progress");
    h.drain_publish_idle_dispatches();
    let records = h
        .session_runtime
        .agent_store
        .agent_events(&durable_agent_id)
        .expect("durable records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id == old_prompt_id
                        && terminated.reason
                            == tau_proto::AgentPromptTerminationReason::Stale
            ))
            .count(),
        1,
        "the immutable original Stale commits exactly once"
    );
    assert!(
        records.iter().all(|record| !matches!(
            &record.event,
            Event::AgentPromptTerminated(terminated)
                if terminated.agent_prompt_id.as_str() == "forged-replacement"
        )),
        "interception cannot replace the exact supersession terminal"
    );
    let new_prompt = read_nth_prompt_created(&h, 1);
    assert_ne!(new_prompt.agent_prompt_id, old_prompt_id);
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_uncertain_supersessions
            .is_empty()
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// An exact late provider terminal that arrives before a parked HumanUI Stale
/// commits wins canonical ownership and retires only that supersession.
#[test]
fn provider_terminal_wins_over_parked_human_ui_supersession() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("old owner".to_owned()))
        .expect("dispatch owner");
    let old_prompt_id = read_nth_prompt_created(&h, 0).agent_prompt_id.clone();
    let provider = h.provider_runtime.pending_prompts[&old_prompt_id].clone();
    connect_test_tool(&mut h, "terminal-race-interceptor");
    h.handle_extension_event(
        "terminal-race-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_TERMINATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_disconnect(&provider);
    submit_authenticated_ui_prompt(
        &mut h,
        crate::parse_agent_id(&durable_agent_id),
        "queued replacement",
        tau_proto::PromptMessageClass::User,
    )
    .expect("submit authenticated UI prompt");
    assert!(h.runtime_io.publication.pending_intercept.is_some());

    h.handle_provider_response_finished(provider_text_response(
        &old_prompt_id,
        durable_agent_id.clone(),
        "late exact response",
    ))
    .expect("accept winning terminal");
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_uncertain_supersessions
            .is_empty()
    );
    assert!(
        h.session_runtime
            .agent_store
            .agent_events(&durable_agent_id)
            .expect("records")
            .iter()
            .all(|record| !matches!(
                &record.event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id == old_prompt_id
            )),
        "the canceled Stale never becomes canonical"
    );
    h.shutdown().expect("shutdown");
}

/// A provider terminal report admitted before disconnect remains the exact
/// winning authority while parked, so later HumanUI input cannot publish Stale.
#[test]
fn parked_provider_terminal_report_wins_before_human_ui_supersession() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("old owner".to_owned()))
        .expect("dispatch owner");
    let old_prompt_id = read_nth_prompt_created(&h, 0).agent_prompt_id.clone();
    let provider = h.provider_runtime.pending_prompts[&old_prompt_id].clone();
    connect_test_tool(&mut h, "report-race-interceptor");
    h.handle_extension_event(
        "report-race-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register blocking interceptor");
    h.emit_info("park report behind this observation");
    assert!(h.runtime_io.publication.pending_intercept.is_some());
    h.handle_extension_event_inner(
        &provider,
        Event::ProviderResponseFinishedReported(provider_text_response(
            &old_prompt_id,
            durable_agent_id.clone(),
            "winning parked report",
        )),
    )
    .expect("defer provider terminal report");
    h.handle_disconnect(&provider);
    submit_authenticated_ui_prompt(
        &mut h,
        durable_agent_id.clone(),
        "queued after terminal report",
        tau_proto::PromptMessageClass::User,
    )
    .expect("submit authenticated UI prompt");
    h.handle_extension_event(
        "report-race-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit admitted report after disconnect");
    while matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::HarnessNotice(_))
    ) {
        h.handle_extension_event(
            "report-race-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("drain later notice interception");
    }
    assert!(
        h.session_runtime
            .agent_store
            .agent_events(&durable_agent_id)
            .expect("records")
            .iter()
            .any(|record| matches!(
                &record.event,
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == old_prompt_id
            )),
        "the admitted report remains canonical terminal authority"
    );
    assert!(
        h.session_runtime
            .agent_store
            .agent_events(&durable_agent_id)
            .expect("records")
            .iter()
            .all(|record| !matches!(
                &record.event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id == old_prompt_id
            )),
        "no competing Stale commits"
    );
    h.shutdown().expect("shutdown");
}

/// A non-owning provider's deferred report does not block the exact HumanUI
/// Stale and its rejection wakes the Ready supersession without another input.
#[test]
fn nonowning_deferred_provider_report_does_not_wedge_human_ui_supersession() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("old owner".to_owned()))
        .expect("dispatch owner");
    let old_prompt_id = read_nth_prompt_created(&h, 0).agent_prompt_id.clone();
    let owning_provider = h.provider_runtime.pending_prompts[&old_prompt_id].clone();
    connect_ready_configured_extension(
        &mut h,
        "nonowning-provider",
        "nonowning-provider",
        tau_proto::ClientKind::Provider,
    );
    let nonowner = crate::test_connection_id("nonowning-provider");
    connect_test_tool(&mut h, "nonowner-race-interceptor");
    h.handle_extension_event(
        "nonowner-race-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register blocking interceptor");
    h.emit_info("park nonowner report behind this observation");
    h.handle_extension_event_inner(
        &nonowner,
        Event::ProviderResponseFinishedReported(provider_text_response(
            &old_prompt_id,
            durable_agent_id.clone(),
            "spoofed terminal",
        )),
    )
    .expect("defer nonowning report");
    h.handle_disconnect(&owning_provider);
    submit_authenticated_ui_prompt(
        &mut h,
        durable_agent_id.clone(),
        "fresh HumanUI prompt",
        tau_proto::PromptMessageClass::User,
    )
    .expect("submit authenticated UI prompt");
    h.handle_extension_event(
        "nonowner-race-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release blocking observation");
    while matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::HarnessNotice(_))
    ) {
        h.handle_extension_event(
            "nonowner-race-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("drain later notice");
    }
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent_events(&durable_agent_id)
            .expect("records")
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id == old_prompt_id
                        && terminated.reason
                            == tau_proto::AgentPromptTerminationReason::Stale
            ))
            .count(),
        1,
        "nonowner report retirement wakes the exact Stale"
    );
    h.shutdown().expect("shutdown");
}

/// Exercise retirement of an admitted exact provider report while cancellation
/// and HumanUI supersession wait behind it.
fn assert_retired_exact_provider_report_redrives_cancel(replace: bool) {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("old owner".to_owned()))
        .expect("dispatch owner");
    let old_prompt_id = read_nth_prompt_created(&h, 0).agent_prompt_id.clone();
    let provider = h.provider_runtime.pending_prompts[&old_prompt_id].clone();
    connect_test_tool(&mut h, "stale-race-interceptor");
    h.handle_extension_event(
        "stale-race-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_TERMINATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register Stale interceptor");
    connect_test_tool(&mut h, "report-drop-interceptor");
    h.handle_extension_event(
        "report-drop-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED_REPORTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register report interceptor");

    h.handle_disconnect(&provider);
    connect_ready_configured_extension(
        &mut h,
        "late-provider",
        "late-provider",
        tau_proto::ClientKind::Provider,
    );
    let late_provider = crate::test_connection_id("late-provider");
    submit_authenticated_ui_prompt(
        &mut h,
        durable_agent_id.clone(),
        "fresh HumanUI prompt",
        tau_proto::PromptMessageClass::User,
    )
    .expect("park HumanUI Stale");
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "stale-race-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("retain exact Stale after admission pressure");
    assert!(matches!(
        h.prompt_coordination
            .prompt_runtime
            .pending_uncertain_supersessions[&cid]
            .phase,
        super::super::super::prompt_runtime_state::UncertainSupersessionPhase::RetainedRetry
    ));
    assert!(matches!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .get(&cid),
        Some(super::super::super::interception::AgentPublishCompletion::UncertainSupersession {
            agent_prompt_id,
            ..
        }) if agent_prompt_id == &old_prompt_id
    ));
    h.handle_disconnect(&crate::test_connection_id("stale-race-interceptor"));
    // Model the admission-time route proof held by a terminal already read
    // from the prompt's provider.
    h.provider_runtime
        .pending_prompts
        .insert(old_prompt_id.clone(), late_provider.clone());
    h.handle_extension_event_inner(
        &late_provider,
        Event::ProviderResponseFinishedReported(provider_text_response(
            &old_prompt_id,
            durable_agent_id.clone(),
            "terminal dropped by policy",
        )),
    )
    .expect("park exact provider terminal report");
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::ProviderResponseFinishedReported(response))
            if response.agent_prompt_id == old_prompt_id
    ));
    h.handle_disconnect(&late_provider);
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(durable_agent_id.clone()),
            agent_prompt_id: None,
        },
    );
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "report-drop-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: if replace {
                InterceptAction::Pass(Some(Box::new(Event::ProviderResponseFinishedReported(
                    provider_text_response(
                        &test_agent_prompt_id("replacement-report-prompt"),
                        durable_agent_id.clone(),
                        "replacement loses captured route authority",
                    ),
                ))))
            } else {
                InterceptAction::Drop
            },
        })),
    )
    .expect("retire admitted terminal report");
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_cancel
            .is_some(),
        "admission pressure retains the exact cancellation owner"
    );
    h.session_runtime
        .persistence_owner
        .as_ref()
        .expect("persistence owner")
        .signal_capacity_ready_for_test();
    let mut served_clients = 0;
    let mut exit_on_disconnect = false;
    let mut ever_attached = false;
    h.handle_runtime_event(
        HarnessEvent::Command(HarnessCommand::SemanticPersistenceProgress),
        &mut served_clients,
        &mut exit_on_disconnect,
        &mut ever_attached,
    )
    .expect("capacity progress retries exact cancellation");

    assert_eq!(
        h.session_runtime
            .agent_store
            .agent_events(&durable_agent_id)
            .expect("records")
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id == old_prompt_id
                        && terminated.reason
                            == tau_proto::AgentPromptTerminationReason::Canceled
            ))
            .count(),
        1,
        "report retirement wakes the exact cancellation without another input"
    );
    assert!(
        h.session_runtime
            .agent_store
            .agent_events(&durable_agent_id)
            .expect("records")
            .iter()
            .all(|record| !matches!(
                &record.event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id == old_prompt_id
                        && terminated.reason == tau_proto::AgentPromptTerminationReason::Stale
            )),
        "cancellation remains stronger than the queued HumanUI supersession"
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_uncertain_supersessions
            .is_empty()
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty(),
        "canonical cancellation retires the exact retained Stale completion"
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_cancel
            .is_none()
    );
    h.shutdown().expect("shutdown");
}

/// Dropping an admitted raw terminal re-drives exact cancellation before Stale.
#[test]
fn dropped_exact_provider_report_redrives_cancel_before_human_ui_supersession() {
    assert_retired_exact_provider_report_redrives_cancel(false);
}

/// Replacing an admitted raw terminal with a different prompt also re-drives
/// exact cancellation before Stale.
#[test]
fn replaced_exact_provider_report_redrives_cancel_before_human_ui_supersession() {
    assert_retired_exact_provider_report_redrives_cancel(true);
}

/// HumanUI input accepted before restored activation handling coalesces behind
/// the exact replay Stale owner instead of publishing a duplicate terminal.
#[test]
fn replay_uncertain_stale_owner_coalesces_human_ui_during_initialization_cut() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let replacement_model = h
        .provider_runtime
        .model_info
        .values()
        .find(|model| model.id == tau_proto::ModelId::from("test/model"))
        .expect("test model")
        .clone();
    let cid = ensure_test_user_agent(&mut h);
    let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("old owner".to_owned()))
        .expect("dispatch owner");
    let old_prompt_id = read_nth_prompt_created(&h, 0).agent_prompt_id.clone();
    let provider = h.provider_runtime.pending_prompts[&old_prompt_id].clone();
    h.handle_disconnect(&provider);
    connect_ready_configured_extension(
        &mut h,
        "replay-replacement-provider",
        "replay-replacement-provider",
        tau_proto::ClientKind::Provider,
    );
    h.publish_provider_models_update(
        &crate::test_connection_id("replay-replacement-provider"),
        crate::test_extension_name("replay-replacement-provider"),
        tau_proto::ProviderModelsDeclared {
            models: vec![replacement_model],
        },
    );
    h.prompt_coordination
        .prompt_runtime
        .pending_replay_uncertain_stale
        .insert(
            cid.clone(),
            tau_proto::AgentPromptTerminated {
                automatic_compaction_decision: None,
                agent_id: durable_agent_id.clone(),
                agent_prompt_id: old_prompt_id.clone(),
                reason: tau_proto::AgentPromptTerminationReason::Stale,
                originator: tau_proto::PromptOriginator::User,
            },
        );
    connect_test_tool(&mut h, "replay-stale-interceptor");
    h.handle_extension_event(
        "replay-stale-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_TERMINATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register replay Stale interceptor");

    let session_id = h.session_runtime.current_session_id.clone();
    h.session_runtime.turn_state = TurnState::InitializingSession {
        session_id: session_id.clone(),
        reason: tau_proto::SessionStartReason::Resume,
        waiting_on: path_std_collections::HashSet::new(),
    };
    submit_authenticated_ui_prompt(
        &mut h,
        durable_agent_id.clone(),
        "queued during replay activation",
        tau_proto::PromptMessageClass::User,
    )
    .expect("submit authenticated UI prompt");
    assert!(
        h.runtime_io.publication.pending_intercept.is_none(),
        "initialization retains the Ready Stale until repair completes"
    );
    h.try_publish_ready_uncertain_supersession(&cid);
    assert!(
        h.runtime_io.publication.pending_intercept.is_none(),
        "incidental singular wakes cannot bypass the initialization cut"
    );
    h.complete_session_init(session_id, tau_proto::SessionStartReason::Resume)
        .expect("complete session initialization");
    assert!(
        matches!(
            h.runtime_io
                .publication
                .pending_intercept
                .as_ref()
                .map(|pending| &pending.event),
            Some(Event::AgentPromptTerminated(terminated))
                if terminated.agent_prompt_id == old_prompt_id
        ),
        "the explicit post-repair drive publishes the retained Stale"
    );
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "replay-stale-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("reject replay Stale admission");
    assert!(matches!(
        h.prompt_coordination
            .prompt_runtime
            .pending_uncertain_supersessions[&cid]
            .phase,
        super::super::super::prompt_runtime_state::UncertainSupersessionPhase::RetainedRetry
    ));
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_replay_uncertain_stale
            .is_empty(),
        "the replay owner transfers into the retained supersession envelope"
    );
    assert!(
        h.session_runtime
            .agent_store
            .agent_events(&durable_agent_id)
            .expect("records")
            .iter()
            .all(|record| !matches!(
                &record.event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id == old_prompt_id
            )),
        "HumanUI does not publish a competing Stale"
    );

    h.session_runtime
        .persistence_owner
        .as_ref()
        .expect("persistence owner")
        .signal_capacity_ready_for_test();
    let mut served_clients = 0;
    let mut exit_on_disconnect = false;
    let mut ever_attached = false;
    h.handle_runtime_event(
        HarnessEvent::Command(HarnessCommand::SemanticPersistenceProgress),
        &mut served_clients,
        &mut exit_on_disconnect,
        &mut ever_attached,
    )
    .expect("capacity progress retries replay Stale");
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent_events(&durable_agent_id)
            .expect("records")
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id == old_prompt_id
            ))
            .count(),
        1,
        "the replay owner commits exactly once"
    );
    let successor = read_nth_prompt_created(&h, 1);
    assert_ne!(
        successor.agent_prompt_id, old_prompt_id,
        "FIFO dispatch uses a fresh prompt id"
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_uncertain_supersessions
            .is_empty()
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// Cold replay observes only the written journal prefix: a lost Stale restores
/// uncertainty, while a written Stale closes the owner without reconstructing
/// the process-local HumanUI queue.
#[test]
fn human_ui_supersession_crash_tail_respects_written_stale_prefix() {
    for retain_stale in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        let (durable_agent_id, old_prompt_id, cut_records) = {
            let mut h = quiet_provider_harness(&state).expect("start");
            let cid = ensure_test_user_agent(&mut h);
            let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
            h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("old owner".to_owned()))
                .expect("dispatch owner");
            let old_prompt_id = read_nth_prompt_created(&h, 0).agent_prompt_id.clone();
            let provider = h.provider_runtime.pending_prompts[&old_prompt_id].clone();
            h.handle_disconnect(&provider);
            let before_stale = h
                .session_runtime
                .agent_store
                .agent_events(&durable_agent_id)
                .expect("pre-Stale records")
                .to_vec();
            submit_authenticated_ui_prompt(
                &mut h,
                durable_agent_id.clone(),
                "process-local crash-tail text",
                tau_proto::PromptMessageClass::User,
            )
            .expect("submit HumanUI supersession");
            let after_stale = h
                .session_runtime
                .agent_store
                .agent_events(&durable_agent_id)
                .expect("post-Stale records")
                .to_vec();
            let stale_index = after_stale
                .iter()
                .position(|record| {
                    matches!(
                        &record.event,
                        Event::AgentPromptTerminated(terminated)
                            if terminated.agent_prompt_id == old_prompt_id
                                && terminated.reason
                                    == tau_proto::AgentPromptTerminationReason::Stale
                    )
                })
                .expect("canonical Stale");
            let cut_records = if retain_stale {
                after_stale[..=stale_index].to_vec()
            } else {
                before_stale
            };
            h.shutdown().expect("flush seed before crash-tail rewrite");
            (durable_agent_id, old_prompt_id, cut_records)
        };

        let journal_path = state
            .join("agents")
            .join(durable_agent_id.as_str())
            .join("events.cbor");
        let mut journal = File::create(&journal_path).expect("rewrite crash-tail prefix");
        for record in &cut_records {
            let mut encoded = Vec::new();
            ciborium::into_writer(record, &mut encoded).expect("encode cut record");
            journal
                .write_all(&(encoded.len() as u64).to_le_bytes())
                .expect("write record length");
            journal.write_all(&encoded).expect("write cut record");
        }
        journal.sync_all().expect("sync crash-tail prefix");

        let mut restored =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("cold restore");
        let restored_cid = restored
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(durable_agent_id.as_str())
            .cloned()
            .expect("restored route");
        let records = restored
            .session_runtime
            .agent_store
            .agent_events(&durable_agent_id)
            .expect("restored records");
        assert!(
            records.starts_with(&cut_records),
            "cold replay must preserve the exact written prefix"
        );
        assert!(
            records[cut_records.len()..]
                .iter()
                .all(|record| matches!(record.event, Event::AgentInitializationContextSet(_))),
            "resume may append only its initialization replacement"
        );
        let stale_count = records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentPromptTerminated(terminated)
                        if terminated.agent_prompt_id == old_prompt_id
                            && terminated.reason
                                == tau_proto::AgentPromptTerminationReason::Stale
                )
            })
            .count();
        assert_eq!(stale_count, usize::from(retain_stale));
        assert_eq!(
            restored
                .session_runtime
                .agent_store
                .agent(&durable_agent_id)
                .and_then(|tree| tree.marked_inference_through(&old_prompt_id))
                .is_some(),
            !retain_stale
        );
        assert_eq!(
            matches!(
                &restored.agent_runtime.agent_registry.agents[&restored_cid]
                    .dispatch
                    .activation_dispatch,
                crate::agent::ActivationDispatchState::DispatchUncertain {
                    owner: crate::agent::InferenceCheckpointOwner::Inference,
                    agent_prompt_id,
                    ..
                } if agent_prompt_id == &old_prompt_id
            ),
            !retain_stale,
            "only the prefix without written Stale restores the exact old owner"
        );
        if retain_stale {
            assert!(matches!(
                restored.agent_runtime.agent_registry.agents[&restored_cid]
                    .dispatch
                    .activation_dispatch,
                crate::agent::ActivationDispatchState::None
            ));
        }
        assert!(
            restored.agent_runtime.agent_registry.agents[&restored_cid]
                .dispatch
                .pending_prompts
                .is_empty(),
            "replay never reconstructs process-local queued prompt text"
        );
        assert!(records.iter().all(|record| {
            !matches!(
                &record.event,
                Event::AgentUserMessageInjected(message)
                    if message.text == "process-local crash-tail text"
            )
        }));
        assert!(
            restored
                .prompt_coordination
                .prompt_runtime
                .pending_replay_uncertain_stale
                .is_empty()
        );
        assert!(
            restored
                .prompt_coordination
                .prompt_runtime
                .pending_uncertain_supersessions
                .is_empty()
        );
        assert!(
            restored
                .prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .is_empty()
        );
        restored.shutdown().expect("shutdown restored cut");
    }
}

/// A retained manual-compaction start installed after HumanUI Stale
/// interception gains priority before semantic admission.
#[test]
fn retained_manual_compaction_start_while_human_ui_stale_is_parked_wins() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let replacement_model = h
        .provider_runtime
        .model_info
        .values()
        .find(|model| model.id == tau_proto::ModelId::from("test/model"))
        .expect("test model")
        .clone();
    let cid = ensure_test_user_agent(&mut h);
    let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("old owner".to_owned()))
        .expect("dispatch owner");
    let old_prompt_id = read_nth_prompt_created(&h, 0).agent_prompt_id.clone();
    let provider = h.provider_runtime.pending_prompts[&old_prompt_id].clone();
    h.handle_disconnect(&provider);
    connect_ready_configured_extension(
        &mut h,
        "replacement-provider",
        "replacement-provider",
        tau_proto::ClientKind::Provider,
    );
    h.publish_provider_models_update(
        &crate::test_connection_id("replacement-provider"),
        crate::test_extension_name("replacement-provider"),
        tau_proto::ProviderModelsDeclared {
            models: vec![replacement_model],
        },
    );
    connect_test_tool(&mut h, "manual-race-interceptor");
    h.handle_extension_event(
        "manual-race-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_TERMINATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    submit_authenticated_ui_prompt(
        &mut h,
        durable_agent_id.clone(),
        "queued behind manual compaction",
        tau_proto::PromptMessageClass::User,
    )
    .expect("submit authenticated UI prompt");
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::AgentPromptTerminated(terminated))
            if terminated.agent_prompt_id == old_prompt_id
    ));
    h.prompt_coordination
        .compaction_runtime
        .rejected_ui_starts
        .insert(
            cid.clone(),
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: durable_agent_id.clone(),
                transaction_id: tau_proto::CompactionTransactionId::parse(
                    "ct-retained-manual-start",
                )
                .expect("transaction"),
                compact_prompt_id: test_agent_prompt_id("retained-manual-start"),
                cut: tau_proto::AgentHead::Root,
                resume_through: None,
                model: "test/model".into(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: tau_proto::PromptOriginator::User,
                supersedes: None,
                trigger: tau_proto::StandaloneCompactionTrigger::ManualUi {
                    request_id: tau_proto::CompactionRequestId::parse("cr-retained-manual-start")
                        .expect("request"),
                },
            }),
        );
    h.handle_extension_event(
        "manual-race-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release Stale after compaction claim");

    assert!(
        h.session_runtime
            .agent_store
            .agent_events(&durable_agent_id)
            .expect("records")
            .iter()
            .all(|record| !matches!(
                &record.event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id == old_prompt_id
            )),
        "manual compaction authority prevents Stale commit"
    );
    assert!(matches!(
        h.prompt_coordination
            .prompt_runtime
            .pending_uncertain_supersessions[&cid]
            .phase,
        super::super::super::prompt_runtime_state::UncertainSupersessionPhase::Ready
    ));
    assert!(
        !h.prompt_coordination
            .compaction_runtime
            .rejected_ui_starts
            .is_empty(),
        "retained manual start keeps target authority"
    );
    h.shutdown().expect("shutdown");
}

/// Provider loss must preserve a transaction-owned checkpoint even when an
/// activating input is deferred; only ordinary inference may close as stale.
#[test]
fn provider_loss_keeps_standalone_checkpoint_uncertain_with_deferred_input() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let (agent_id, transaction_id, prompt_id, through) =
        seed_restored_compaction_checkpoint(&mut h, &cid, &"test/model".into(), "ct-provider-loss");
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
    h.publish_for_agent(&cid, checkpoint);
    let provider = h
        .extension_connection_id("provider")
        .expect("provider")
        .to_owned();
    h.provider_runtime
        .pending_prompts
        .insert(prompt_id.clone(), crate::test_connection_id(&provider));
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(prompt_id.clone(), cid.clone());
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .pending_message_wakes
        .push_back(crate::agent::PendingMessageWake {
            source: path_crate_agent::PendingMessageWakeSource::MessageFact {
                durable_event_seq: tau_core::PersistedAgentEventSeq::new(0),
            },
            node_id: None,
            activation_observation: None,
            source_observation: None,
            delivery_schedule: None,
        });

    h.handle_disconnect(&crate::test_connection_id(&provider));

    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::DispatchUncertain {
            owner: crate::agent::InferenceCheckpointOwner::Standalone { ref id },
            ref agent_prompt_id,
            ..
        } if id == &transaction_id && agent_prompt_id == &prompt_id
    ));
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::AgentPromptTerminated(terminated)
                if terminated.agent_prompt_id == prompt_id
        )
    }));
    assert!(matches!(
        h.session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .and_then(tau_core::AgentTree::standalone_compaction_recovery),
        Some(tau_core::StandaloneCompactionRecovery::DispatchUncertain(_))
    ));
    h.shutdown().expect("shutdown");
}

/// Harness-owned estimates start at zero for a newly loaded runtime agent and
/// accumulate each accepted usage record with that serving model's current
/// explicit or fallback prices.
#[test]
fn agent_stats_accumulate_runtime_estimated_api_cost_by_serving_model() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    assert_eq!(
        h.agent_stats_snapshot(&cid)
            .expect("initial stats")
            .estimated_api_cost,
        tau_proto::EstimatedApiCost::default()
    );

    let model_a = tau_proto::ModelId::from("provider-a/model");
    let mut info_a = provider_model_info(model_a.clone(), 1_000);
    info_a.est_uncached_input_cost_1m_usd = tau_proto::EstimatedUsdPerMillion::checked_from_usd(1);
    info_a.est_cached_input_cost_1m_usd =
        Some(tau_proto::EstimatedUsdPerMillion::from_micro_usd(500_000));
    info_a.est_output_cost_1m_usd = tau_proto::EstimatedUsdPerMillion::checked_from_usd(2);
    h.provider_runtime
        .model_info
        .insert(model_a.clone(), info_a);
    h.prompt_coordination
        .prompt_runtime
        .estimated_cost_rates
        .insert(
            test_agent_prompt_id("cost-prompt"),
            h.provider_runtime.model_info[&model_a].estimated_api_cost_rates(),
        );
    let captured_rates = h.provider_runtime.model_info[&model_a].estimated_api_cost_rates();
    h.provider_runtime
        .model_info
        .get_mut(&model_a)
        .expect("model metadata")
        .est_uncached_input_cost_1m_usd = tau_proto::EstimatedUsdPerMillion::checked_from_usd(10);
    let response = |model: tau_proto::ModelId| tau_proto::ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: Some(tau_proto::ProviderTokenUsage {
            model: Some(model),
            prompt_sent_tokens: 1_000_000,
            prompt_cached_tokens: 0,
            prompt_cache_read_ceiling_tokens: None,
            cache: None,
            response_received_tokens: 0,
            stats: Default::default(),
        }),
        ..provider_text_response(
            &test_agent_prompt_id("cost-prompt"),
            crate::parse_agent_id("main"),
            "ok",
        )
    };

    let mut response_a = response(model_a);
    h.add_finished_response_estimated_cost(&cid, &mut response_a, None, true);
    let mut response_fallback = response(tau_proto::ModelId::from("local/unpriced"));
    h.add_finished_response_estimated_cost(&cid, &mut response_fallback, None, true);
    assert_eq!(response_a.estimated_api_cost_rates, Some(captured_rates));
    assert_eq!(
        response_a.estimated_api_cost_increment,
        Some(tau_proto::EstimatedApiCost::from_picodollars(
            1_000_000_000_000
        ))
    );

    assert_eq!(
        h.agent_stats_snapshot(&cid)
            .expect("updated stats")
            .estimated_api_cost
            .as_picodollars(),
        6_000_000_000_000,
        "dispatch-captured $1 remains unchanged, plus universal fallback $5"
    );
    h.shutdown().expect("shutdown");
}

/// An accepted child provider response charges the child self estimate and its
/// loaded authenticated creator's inclusive subtree estimate before publishing
/// both complete stats snapshots.
#[test]
fn accepted_provider_usage_updates_creator_cost_stats() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _delegate = connect_test_tool(&mut h, "conn-delegate");
    let parent_cid = ensure_test_user_agent(&mut h);
    let parent_id = durable_agent_id_for_conversation(&h, &parent_cid);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("creator-cost-call".into(), parent_cid);
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "creator-cost-child".to_owned(),
            instruction: "charge this child".to_owned(),
            role: None,
            input_stats: Default::default(),
            tool_call_id: Some("creator-cost-call".into()),
            task_name: Some("charged child".to_owned()),
        },
    )
    .expect("start authenticated child");
    let child_cid = ext_query_cid(&h, "creator-cost-child").expect("child conversation");
    let child_id = durable_agent_id_for_conversation(&h, &child_cid);

    let model = tau_proto::ModelId::from("provider/charged");
    let mut model_info = provider_model_info(model.clone(), 1_000_000);
    model_info.est_uncached_input_cost_1m_usd =
        tau_proto::EstimatedUsdPerMillion::checked_from_usd(2);
    model_info.est_cached_input_cost_1m_usd =
        tau_proto::EstimatedUsdPerMillion::checked_from_usd(1);
    model_info.est_cache_write_input_cost_1m_usd =
        tau_proto::EstimatedUsdPerMillion::checked_from_usd(3);
    h.provider_runtime
        .model_info
        .insert(model.clone(), model_info);
    let prompt_id = h.agent_runtime.agent_registry.agents[&child_cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("child provider prompt");
    let captured_rates = h.provider_runtime.model_info[&model].estimated_api_cost_rates();
    h.prompt_coordination
        .prompt_runtime
        .estimated_cost_rates
        .insert(prompt_id.clone(), captured_rates);
    let usage = tau_proto::ProviderTokenUsage {
        model: Some(model),
        prompt_sent_tokens: 1_000_000,
        prompt_cached_tokens: 10,
        prompt_cache_read_ceiling_tokens: None,
        cache: Some(Box::new(tau_proto::ProviderCacheUsage {
            read_tokens: Some(100_000),
            write_tokens: Some(900_000),
            refresh_reason: Some(tau_proto::ProviderCacheRefreshReason::OrdinaryRequest),
            expiry_confidence: Some(tau_proto::ProviderCacheExpiryConfidence::Unknown),
            ..Default::default()
        })),
        response_received_tokens: 0,
        stats: Default::default(),
    };
    let mut response = provider_text_response(&prompt_id, child_id.clone(), "charged");
    response.usage = Some(usage.clone());
    h.handle_provider_response_finished(response)
        .expect("accept provider response");

    let child_stats = h.agent_stats_snapshot(&child_id).expect("child stats");
    let parent_stats = h.agent_stats_snapshot(&parent_id).expect("parent stats");
    let expected = tau_proto::EstimatedApiCost::from_picodollars(2_800_000_000_000);
    assert_eq!(child_stats.estimated_api_cost, expected);
    assert_eq!(child_stats.creator_subtree_estimated_api_cost, expected);
    assert_eq!(parent_stats.estimated_api_cost, Default::default());
    assert_eq!(parent_stats.creator_subtree_estimated_api_cost, expected);
    let published = event_log_events(&h);
    let canonical = published
        .iter()
        .find_map(|event| match event {
            Event::ProviderResponseFinished(response) if response.agent_prompt_id == prompt_id => {
                Some(response)
            }
            _ => None,
        })
        .expect("canonical accepted response");
    let canonical_usage = canonical.usage.as_ref().expect("canonical response usage");
    assert_eq!(canonical_usage.prompt_sent_tokens, usage.prompt_sent_tokens);
    assert_eq!(
        canonical_usage.response_received_tokens,
        usage.response_received_tokens
    );
    assert_eq!(canonical_usage.prompt_cached_tokens, 100_000);
    assert_eq!(
        canonical_usage
            .cache
            .as_deref()
            .and_then(|cache| cache.read_tokens),
        Some(100_000)
    );
    assert_eq!(canonical_usage.stats.total.cached_tokens, 100_000);
    assert_eq!(canonical.estimated_api_cost_rates, Some(captured_rates));
    assert_eq!(canonical.estimated_api_cost_increment, Some(expected));
    assert!(
        published
            .iter()
            .any(|event| matches!(event, Event::AgentStatsUpdated(stats)
                if stats.agent_id == child_id
                    && stats.estimated_api_cost == expected
                    && stats.creator_subtree_estimated_api_cost == expected
            ))
    );
    assert!(
        published
            .iter()
            .any(|event| matches!(event, Event::AgentStatsUpdated(stats)
                if stats.agent_id == parent_id
                    && stats.estimated_api_cost == Default::default()
                    && stats.creator_subtree_estimated_api_cost == expected
            ))
    );
    h.shutdown().expect("shutdown");
}

/// A closer committed after a deferred typed receipt materializes that receipt
/// once. A response places it after `R`; durable Canceled/Stale terminals use
/// the accepted fallback parent without adding another closer on replay.
#[test]
fn resume_wakes_once_after_v1_response_or_durable_terminal_fallback() {
    #[derive(Clone, Copy)]
    enum Closer {
        Response,
        Terminal(tau_proto::AgentPromptTerminationReason),
    }
    for closer in [
        Closer::Response,
        Closer::Terminal(tau_proto::AgentPromptTerminationReason::Canceled),
        Closer::Terminal(tau_proto::AgentPromptTerminationReason::Stale),
    ] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        seed_main_agent_loaded(&state);
        let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
        let owner = test_agent_prompt_id(match closer {
            Closer::Response => "ap-v1-response-before-restart",
            Closer::Terminal(tau_proto::AgentPromptTerminationReason::Canceled) => {
                "ap-v1-canceled-before-restart"
            }
            Closer::Terminal(tau_proto::AgentPromptTerminationReason::Stale) => {
                "ap-v1-stale-before-restart"
            }
        });
        let mut store = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
        append_seed_agent_event(
            &mut store,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: agent_id.clone(),
                text: "H".to_owned(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::Internal,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        );
        let through = store
            .agent("main")
            .and_then(tau_core::AgentTree::head)
            .expect("H node");
        append_seed_agent_event(
            &mut store,
            Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                output_length_continuation: None,
                agent_id: agent_id.clone(),
                transaction_id: None,
                agent_prompt_id: owner.clone(),
                through: tau_proto::AgentHead::Node(through),
                model: Some("echo/model".into()),
                operation: Some(tau_proto::PromptOperation::Inference),
                activation_cut: Some(tau_proto::AgentHead::Root),
            }),
        );
        append_seed_agent_event(
            &mut store,
            Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
                message_id: tau_proto::AgentMessageId::parse("wake-after-closure")
                    .expect("message id"),
                sender_id: tau_proto::AgentId::parse("sender").expect("sender"),
                sender_session_id: None,
                recipient_id: agent_id.clone(),
                kind: tau_proto::AgentMessageKind::Message,
                watch_provider_status: None,
                watch_work_status: None,
                watch_long_wait: None,
                watch_lifecycle: None,
                message: "Q after closure".to_owned(),
            }),
        );
        append_seed_agent_event(
            &mut store,
            match closer {
                Closer::Response => Event::ProviderResponseFinished(provider_text_response(
                    &owner,
                    agent_id.clone(),
                    "R before Q",
                )),
                Closer::Terminal(reason) => {
                    Event::AgentPromptTerminated(tau_proto::AgentPromptTerminated {
                        automatic_compaction_decision: None,
                        agent_id: agent_id.clone(),
                        agent_prompt_id: owner.clone(),
                        reason,
                        originator: tau_proto::PromptOriginator::User,
                    })
                }
            },
        );
        drop(store);

        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let prompt = read_nth_prompt_created(&h, 0);
        let rendered = serde_json::to_string(&prompt.context).expect("context");
        assert_eq!(rendered.matches("Q after closure").count(), 1);
        if matches!(closer, Closer::Response) {
            assert_eq!(rendered.matches("R before Q").count(), 1);
            assert!(rendered.find("R before Q") < rendered.find("Q after closure"));
        }
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
                .count(),
            1
        );
        assert!(
            event_log_events(&h).iter().all(|event| !matches!(
                event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id == owner
            )),
            "replay must not append a second closer"
        );
        h.shutdown().expect("shutdown");
    }
}

/// A finished ordinary response with usage above an alert threshold must route
/// the configured text through the normal durable internal-prompt path.
#[test]
fn finished_response_injects_crossed_context_size_alert() {
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
                message: "compact after this task".to_owned(),
                when: tau_config::settings::ContextPolicyWhen {
                    at: path_tau_config_settings::ContextPolicyPoint::AfterResponse,
                    statuses: None,
                },
            },
        );

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("work".to_owned()))
        .expect("dispatch");
    let prompt = read_nth_prompt_created(&h, 0);
    let mut response =
        provider_text_response(&prompt.agent_prompt_id, prompt.agent_id, "finished work");
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 101,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 2,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(response)
        .expect("finish response");

    let alert_prompt = read_nth_prompt_created(&h, 1);
    let events = event_log_events(&h);
    let response_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::ProviderResponseFinished(finished)
                    if finished.agent_prompt_id == prompt.agent_prompt_id
            )
        })
        .expect("threshold-crossing response");
    let alert_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentPromptSubmitted(submitted)
                    if submitted.text == "compact after this task"
                        && submitted.message_class == tau_proto::PromptMessageClass::Internal
                        && submitted.internal_kind
                            == Some(tau_proto::InternalPromptKind::ContextSizeAlert)
            )
        })
        .expect("durable context-size alert");
    let dispatch_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentPromptCreated(created)
                    if created.agent_prompt_id == alert_prompt.agent_prompt_id
            )
        })
        .expect("alert provider dispatch");
    assert!(
        response_index < alert_index && alert_index < dispatch_index,
        "alert delivery fact must land after the crossing response and before its provider dispatch"
    );
    h.shutdown().expect("shutdown");
}

/// Terminal provider errors may report usage for diagnostics, but must not turn
/// an advisory context alert into autonomous retry work.
#[test]
fn failed_response_does_not_inject_context_size_alert() {
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
                message: "must not continue".to_owned(),
                when: tau_config::settings::ContextPolicyWhen {
                    at: path_tau_config_settings::ContextPolicyPoint::AfterResponse,
                    statuses: None,
                },
            },
        );
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("work".to_owned()))
        .expect("dispatch");
    let prompt = read_nth_prompt_created(&h, 0);
    let mut response = provider_text_response(&prompt.agent_prompt_id, prompt.agent_id, "ignored");
    response.output_items.clear();
    response.stop_reason = tau_proto::ProviderStopReason::Error;
    response.error = Some("terminal failure".to_owned());
    response.failure_kind = Some(tau_proto::ProviderFailureKind::Unknown);
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 101,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 0,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(response)
        .expect("finish response");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == "must not continue"
    )));
    h.shutdown().expect("shutdown");
}
/// Cold replay must not apply usage from an unqualified or different model,
/// and a live per-agent model change clears already-restored usage.
#[test]
fn restored_context_usage_requires_current_model_and_resets_on_model_change() {
    for model in [None, Some("other/model")] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        seed_agent_context_usage(&state, model, 900);
        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let cid = ensure_test_user_agent(&mut h);
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_input_tokens,
            None
        );
        h.shutdown().expect("shutdown");
    }

    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    seed_agent_context_usage(&state, Some("test/model"), 900);
    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let cid = ensure_test_user_agent(&mut h);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        Some(tau_proto::TokenCount::new(900))
    );
    let alternate: tau_proto::ModelId = "test/alternate".into();
    let route = h
        .provider_runtime
        .model_routes
        .get(&"test/model".into())
        .expect("test model route")
        .clone();
    let mut info = h
        .provider_runtime
        .model_info
        .get(&"test/model".into())
        .expect("test model info")
        .clone();
    info.id = alternate.clone();
    info.default_affinity = -1;
    h.provider_runtime.available_models.push(alternate.clone());
    h.provider_runtime
        .model_routes
        .insert(alternate.clone(), route);
    h.provider_runtime
        .model_info
        .insert(alternate.clone(), info);
    h.handle_ui_agent_model_select(
        crate::harness::harness_connection_id(),
        tau_proto::UiAgentModelSelect {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: h.agent_runtime.agent_registry.agents[&cid]
                .identity
                .agent_id
                .as_deref()
                .map(crate::parse_agent_id),
            model: alternate,
        },
    )
    .expect("select alternate model");
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
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_cached_tokens,
        None
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_percent_used,
        None
    );
    h.shutdown().expect("shutdown");
}

/// Ensures relative role updates start from the selected model's effective
/// defaults, then retain out-of-range effort intent while the other level
/// settings saturate through the dispatch path.
#[test]
fn relative_role_updates_use_model_fallbacks_and_saturate() {
    use std::num::NonZeroU8;

    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("start");
    let role = h.config.selected_role.clone();
    let model = h.config.selected_model.clone().expect("selected model");
    let settings = h
        .provider_runtime
        .model_info
        .get_mut(&model)
        .expect("selected model settings");
    settings.efforts = tau_proto::ReasoningEffortCapability::mapped(vec![
        tau_proto::NativeReasoningEffort::None,
        tau_proto::NativeReasoningEffort::Low,
        tau_proto::NativeReasoningEffort::Medium,
        tau_proto::NativeReasoningEffort::High,
        tau_proto::NativeReasoningEffort::Max,
    ]);
    settings.verbosities = vec![
        tau_proto::Verbosity::Low,
        tau_proto::Verbosity::Medium,
        tau_proto::Verbosity::High,
    ];
    settings.thinking_summaries = vec![
        tau_proto::ThinkingSummary::Off,
        tau_proto::ThinkingSummary::Auto,
        tau_proto::ThinkingSummary::Concise,
        tau_proto::ThinkingSummary::Detailed,
    ];
    let configured_role = h
        .config
        .available_roles
        .get_mut(&role)
        .expect("selected role");
    configured_role.effort = None;
    configured_role.verbosity = None;
    configured_role.thinking_summary = None;

    let one = NonZeroU8::new(1).expect("positive adjustment");
    for action in [
        tau_proto::UiRoleUpdateAction::AdjustEffort {
            adjustment: tau_proto::ReasoningIntensityDelta::new(250_000).expect("nonzero"),
        },
        tau_proto::UiRoleUpdateAction::AdjustVerbosity {
            adjustment: tau_proto::UiRoleSettingAdjustment::Increase(one),
        },
        tau_proto::UiRoleUpdateAction::AdjustThinkingSummary {
            adjustment: tau_proto::UiRoleSettingAdjustment::Increase(one),
        },
    ] {
        h.handle_ui_role_update(
            crate::harness::harness_connection_id(),
            tau_proto::UiRoleUpdate {
                role: role.clone(),
                action,
            },
        )
        .expect("apply relative role update");
    }
    let updated_role = h.config.available_roles.get(&role).expect("updated role");
    assert_eq!(
        updated_role.effort,
        Some(tau_proto::NativeReasoningEffort::High.into())
    );
    assert_eq!(updated_role.verbosity, Some(tau_proto::Verbosity::Medium));
    assert_eq!(
        updated_role.thinking_summary,
        Some(tau_proto::ThinkingSummary::Concise)
    );

    let far = NonZeroU8::new(99).expect("positive adjustment");
    for action in [
        tau_proto::UiRoleUpdateAction::AdjustEffort {
            adjustment: tau_proto::ReasoningIntensityDelta::new(-2_000_000).expect("nonzero"),
        },
        tau_proto::UiRoleUpdateAction::AdjustVerbosity {
            adjustment: tau_proto::UiRoleSettingAdjustment::Decrease(far),
        },
        tau_proto::UiRoleUpdateAction::AdjustThinkingSummary {
            adjustment: tau_proto::UiRoleSettingAdjustment::Decrease(far),
        },
    ] {
        h.handle_ui_role_update(
            crate::harness::harness_connection_id(),
            tau_proto::UiRoleUpdate {
                role: role.clone(),
                action,
            },
        )
        .expect("saturate role setting downward");
    }
    let updated_role = h.config.available_roles.get(&role).expect("updated role");
    assert_eq!(
        updated_role.effort,
        Some(tau_proto::ReasoningIntent::Intensity(
            tau_proto::ReasoningIntensity::from_millionths(-1_250_000)
        ))
    );
    assert_eq!(updated_role.verbosity, Some(tau_proto::Verbosity::Low));
    assert_eq!(
        updated_role.thinking_summary,
        Some(tau_proto::ThinkingSummary::Off)
    );

    for action in [
        tau_proto::UiRoleUpdateAction::AdjustEffort {
            adjustment: tau_proto::ReasoningIntensityDelta::new(2_000_000).expect("nonzero"),
        },
        tau_proto::UiRoleUpdateAction::AdjustVerbosity {
            adjustment: tau_proto::UiRoleSettingAdjustment::Increase(far),
        },
        tau_proto::UiRoleUpdateAction::AdjustThinkingSummary {
            adjustment: tau_proto::UiRoleSettingAdjustment::Increase(far),
        },
    ] {
        h.handle_ui_role_update(
            crate::harness::harness_connection_id(),
            tau_proto::UiRoleUpdate {
                role: role.clone(),
                action,
            },
        )
        .expect("saturate role setting upward");
    }
    let updated_role = h.config.available_roles.get(&role).expect("updated role");
    assert_eq!(
        updated_role.effort,
        Some(tau_proto::ReasoningIntent::Intensity(
            tau_proto::ReasoningIntensity::from_millionths(750_000)
        ))
    );
    assert_eq!(updated_role.verbosity, Some(tau_proto::Verbosity::High));
    assert_eq!(
        updated_role.thinking_summary,
        Some(tau_proto::ThinkingSummary::Detailed)
    );

    let settings = h
        .provider_runtime
        .model_info
        .get_mut(&model)
        .expect("selected model settings");
    settings.efforts = tau_proto::ReasoningEffortCapability::mapped(vec![
        tau_proto::NativeReasoningEffort::Medium,
        tau_proto::NativeReasoningEffort::High,
    ]);
    settings.verbosities = vec![tau_proto::Verbosity::Low, tau_proto::Verbosity::Medium];
    settings.thinking_summaries = vec![
        tau_proto::ThinkingSummary::Off,
        tau_proto::ThinkingSummary::Auto,
        tau_proto::ThinkingSummary::Concise,
    ];
    for action in [
        tau_proto::UiRoleUpdateAction::AdjustEffort {
            adjustment: tau_proto::ReasoningIntensityDelta::new(-250_000).expect("nonzero"),
        },
        tau_proto::UiRoleUpdateAction::AdjustVerbosity {
            adjustment: tau_proto::UiRoleSettingAdjustment::Decrease(one),
        },
        tau_proto::UiRoleUpdateAction::AdjustThinkingSummary {
            adjustment: tau_proto::UiRoleSettingAdjustment::Decrease(one),
        },
    ] {
        h.handle_ui_role_update(
            crate::harness::harness_connection_id(),
            tau_proto::UiRoleUpdate {
                role: role.clone(),
                action,
            },
        )
        .expect("adjust the model-clamped role setting");
    }
    let updated_role = h.config.available_roles.get(&role).expect("updated role");
    assert_eq!(
        updated_role.effort,
        Some(tau_proto::NativeReasoningEffort::Medium.into())
    );
    assert_eq!(updated_role.verbosity, Some(tau_proto::Verbosity::Low));
    assert_eq!(
        updated_role.thinking_summary,
        Some(tau_proto::ThinkingSummary::Off)
    );

    let settings = h
        .provider_runtime
        .model_info
        .get_mut(&model)
        .expect("selected model settings");
    settings.efforts = tau_proto::ReasoningEffortCapability::mapped(vec![
        tau_proto::NativeReasoningEffort::None,
        tau_proto::NativeReasoningEffort::Low,
        tau_proto::NativeReasoningEffort::Medium,
        tau_proto::NativeReasoningEffort::High,
        tau_proto::NativeReasoningEffort::Max,
    ]);
    settings.verbosities = vec![
        tau_proto::Verbosity::Low,
        tau_proto::Verbosity::Medium,
        tau_proto::Verbosity::High,
    ];
    settings.thinking_summaries = vec![
        tau_proto::ThinkingSummary::Off,
        tau_proto::ThinkingSummary::Auto,
        tau_proto::ThinkingSummary::Concise,
        tau_proto::ThinkingSummary::Detailed,
    ];
    assert_eq!(
        h.selected_model_params().effort,
        tau_proto::ReasoningSelection::native(tau_proto::NativeReasoningEffort::Medium)
    );
    assert_eq!(
        h.selected_model_params().verbosity,
        tau_proto::Verbosity::Low
    );
    assert_eq!(
        h.selected_model_params().thinking_summary,
        tau_proto::ThinkingSummary::Off
    );
    h.shutdown().expect("shutdown");
}

/// Role model overrides and fallback deletion must invalidate usage from the
/// previous resolved model, while an explicit per-agent override remains
/// authoritative.
#[test]
fn role_model_updates_reconcile_loaded_agent_context_usage() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    seed_agent_context_usage(&state, Some("test/model"), 900);
    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let cid = ensure_test_user_agent(&mut h);
    let role = h.config.selected_role.clone();
    let alternate: tau_proto::ModelId = "test/alternate".into();
    let route = h
        .provider_runtime
        .model_routes
        .get(&"test/model".into())
        .expect("test model route")
        .clone();
    let mut info = h
        .provider_runtime
        .model_info
        .get(&"test/model".into())
        .expect("test model info")
        .clone();
    info.id = alternate.clone();
    info.default_affinity = -1;
    h.provider_runtime.available_models.push(alternate.clone());
    h.provider_runtime
        .model_routes
        .insert(alternate.clone(), route);
    h.provider_runtime
        .model_info
        .insert(alternate.clone(), info);

    h.handle_ui_role_update(
        crate::harness::harness_connection_id(),
        tau_proto::UiRoleUpdate {
            role: role.clone(),
            action: tau_proto::UiRoleUpdateAction::SetModel {
                model: Some(alternate.clone()),
            },
        },
    )
    .expect("set role model");
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
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_usage_model,
        None
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_cached_tokens,
        None
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_percent_used,
        None
    );
    h.rehydrate_agents_from_session();
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        None,
        "same-daemon rehydrate must not revive usage from the prior model"
    );

    {
        let conv = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        conv.execution.context_input_tokens = Some(tau_proto::TokenCount::new(800));
        conv.execution.context_usage_head = conv.identity.head;
        conv.execution.context_usage_model = Some(alternate.clone());
        conv.execution.context_usage_prompt_id =
            Some(test_agent_prompt_id("ap-test-provider-usage"));
        conv.execution.context_cached_tokens = Some(tau_proto::TokenCount::new(400));
        conv.execution.context_percent_used = Some(80);
    }
    h.handle_ui_role_update(
        crate::harness::harness_connection_id(),
        tau_proto::UiRoleUpdate {
            role: role.clone(),
            action: tau_proto::UiRoleUpdateAction::Delete,
        },
    )
    .expect("delete role override");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        None
    );

    {
        let conv = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        conv.identity.model_override = Some("test/model".into());
        conv.execution.context_input_tokens = Some(tau_proto::TokenCount::new(700));
        conv.execution.context_usage_head = conv.identity.head;
        conv.execution.context_usage_model = Some("test/model".into());
        conv.execution.context_usage_prompt_id =
            Some(test_agent_prompt_id("ap-test-provider-usage"));
        conv.execution.context_cached_tokens = Some(tau_proto::TokenCount::new(350));
        conv.execution.context_percent_used = Some(70);
    }
    h.handle_ui_role_update(
        crate::harness::harness_connection_id(),
        tau_proto::UiRoleUpdate {
            role,
            action: tau_proto::UiRoleUpdateAction::SetModel {
                model: Some(alternate),
            },
        },
    )
    .expect("set role model under explicit agent override");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        Some(tau_proto::TokenCount::new(700))
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_usage_model
            .as_ref(),
        Some(&tau_proto::ModelId::from("test/model"))
    );
    h.shutdown().expect("shutdown");
}

/// Provider cache reads greater than sent input must clamp once before both the
/// live context baseline and the canonical terminal response observe them.
#[test]
fn finished_response_normalizes_cached_usage_before_context_update() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("usage".to_owned()))
        .expect("dispatch");
    let prompt = read_nth_prompt_created(&h, 0);
    let mut response = provider_text_response(&prompt.agent_prompt_id, prompt.agent_id, "done");
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 10,
        prompt_cached_tokens: 100,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 1,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(response)
        .expect("finish response");

    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        Some(tau_proto::TokenCount::new(10))
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_cached_tokens,
        Some(tau_proto::TokenCount::new(10))
    );
    let canonical = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::ProviderResponseFinished(response)
                if response.agent_prompt_id == prompt.agent_prompt_id =>
            {
                Some(response)
            }
            _ => None,
        })
        .expect("canonical terminal response");
    assert_eq!(
        canonical
            .usage
            .expect("canonical usage")
            .prompt_cached_tokens,
        10
    );
    h.shutdown().expect("shutdown");
}

#[test]
fn repetition_response_with_output_items_is_cleared_before_persisting() {
    // The harness enforces the repetition-detected empty-output invariant at the
    // provider boundary so a buggy provider cannot smuggle text or tool calls in
    // a loop-guard terminal response.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-provider-repetition-malformed");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    let mut response =
        provider_repetition_response(&spid, tau_proto::AgentId::parse("main").expect("agent id"));
    response.output_items = provider_text_response(
        &spid,
        tau_proto::AgentId::parse("main").expect("agent id"),
        "this text must be discarded",
    )
    .output_items;

    h.handle_provider_response_finished(response)
        .expect("response handled");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderResponseFinished(finished)
            if finished
                .output_items
                .iter()
                .any(|item| matches!(item, ContextItem::Message(_)))
    )));
}

/// A provider terminal that races after committed unload must consume stale
/// prompt correlation without publishing a response or panicking. Re-delivery
/// remains an idempotent duplicate.
#[test]
fn provider_completion_after_agent_unload_is_discarded_idempotently() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let prompt_id: AgentPromptId = test_agent_prompt_id("sp-late-after-unload");
    seed_agent_thinking(&mut h, &cid, prompt_id.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(prompt_id.clone(), cid.clone());
    let response = provider_text_response(
        &prompt_id,
        durable_agent_id_for_conversation(&h, &cid),
        "must be discarded",
    );

    h.remove_agent(&cid);
    assert!(!h.agent_runtime.agent_registry.agents.contains_key(&cid));
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .agents
            .contains_key(&prompt_id)
    );

    h.handle_provider_response_finished(response.clone())
        .expect("late completion discarded");
    h.handle_provider_response_finished(response)
        .expect("duplicate late completion discarded");

    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .agents
            .contains_key(&prompt_id)
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderResponseFinished(finished)
            if finished.agent_prompt_id == prompt_id
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.level == tau_proto::NoticeLevel::Info
                && notice.message.contains("after owner unload")
                && notice.message.contains(prompt_id.as_str())
    )));
}
#[test]
fn chained_sub_chunk_cacheable_tokens_does_not_emit_diagnostic() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    h.submit_user_prompt(test_session_id("s1"), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (Some(500), Some(0), None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt(test_session_id("s1"), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);
    let spid2 = prompt2.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid2,
        agent_id: prompt2.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "second answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (Some(500), Some(0), None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: Some("resp_def".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish second");

    let mut cursor = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(cursor) {
        cursor = entry.seq.next();
        assert!(
            !matches!(entry.event, Event::ProviderCacheMissDiagnostic(_)),
            "sub-cache-chunk turn must not emit cache miss diagnostic"
        );
    }

    h.shutdown().expect("shutdown");
}

/// Changing role-derived model parameters mid-conversation must bust the chain.
/// The Codex Responses upstream stored its reasoning state against
/// the *previous* turn's effort/verbosity/thinking-summary; sending
/// a `previous_response_id` from a request whose non-input fields
/// drifted would silently decohere the model's reasoning. The
/// fingerprint check catches this before the round-trip — mirrors
/// Pi's `requestBodiesMatchExceptInput`.
#[test]
fn params_drift_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    h.config
        .available_roles
        .get_mut(&h.config.selected_role.clone())
        .expect("selected role")
        .effort = Some(tau_proto::NativeReasoningEffort::Low.into());

    h.submit_user_prompt(test_session_id("s1"), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // User dials effort up between turns by updating the selected role override.
    h.config
        .available_roles
        .get_mut(&h.config.selected_role.clone())
        .expect("selected role")
        .effort = Some(tau_proto::NativeReasoningEffort::High.into());

    h.submit_user_prompt(test_session_id("s1"), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );
}

/// Counterpart: when the per-request fingerprint inputs *don't*
/// change between turns, the chain anchor must remain valid. Locks
/// in the "compute fingerprint over (system_prompt, tools, params)"
/// surface — if a future change quietly mixes in some other input
/// that drifts across turns (e.g. cwd, current date, session id),
/// this test starts failing.
#[test]
fn stable_params_preserve_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    h.submit_user_prompt(test_session_id("s1"), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: Some(responses_backend()),
        provider_attempt: Default::default(),
        provider_response_id: Some("resp_xyz".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt(test_session_id("s1"), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );
}
