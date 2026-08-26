//! Tests for provider responses behavior.

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
        compaction_compacted_input_tokens: None,
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
        compaction_compacted_input_tokens: None,
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
        let journal = state
            .join("agents")
            .join(durable_agent_id.as_str())
            .join("events.cbor");
        let backup = journal.with_extension("cbor.test-backup");
        std::fs::rename(&journal, &backup).expect("park journal");
        std::fs::create_dir(&journal).expect("block journal path");
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
        std::fs::remove_dir(&journal).expect("remove receipt blocker");
        std::fs::rename(&backup, &journal).expect("restore journal");

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
        std::fs::rename(&journal, &backup).expect("park journal for terminal");
        std::fs::create_dir(&journal).expect("block terminal append");
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
        std::fs::remove_dir(&journal).expect("remove terminal blocker");
        std::fs::rename(&backup, &journal).expect("restore journal after terminal failure");

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
    h.add_finished_response_estimated_cost(&cid, &mut response_a, None);
    let mut response_fallback = response(tau_proto::ModelId::from("local/unpriced"));
    h.add_finished_response_estimated_cost(&cid, &mut response_fallback, None);
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
                threshold: 100,
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
                threshold: 100,
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

/// Staggered provider discovery must preserve a qualified baseline while its
/// model is unresolved, validate it when that provider appears, and use it for
/// the first resumed activation's compaction projection.
#[test]
fn restored_usage_survives_staggered_provider_discovery() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let model_b: tau_proto::ModelId = "provider-b/model".into();
    seed_agent_context_usage(&state, Some("provider-b/model"), 900);
    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let role = h.config.selected_role.clone();
    h.config
        .available_roles
        .get_mut(&role)
        .expect("selected role")
        .model = Some(model_b.clone());
    h.rehydrate_agents_from_session();
    let cid = ensure_test_user_agent(&mut h);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        Some(900)
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_usage_model
            .as_ref(),
        Some(&model_b)
    );

    let base_info = h
        .provider_runtime
        .model_info
        .get(&"test/model".into())
        .expect("test model info")
        .clone();
    let mut model_a_info = base_info.clone();
    model_a_info.id = "provider-a/model".into();
    h.set_provider_models(&crate::test_connection_id("provider-a"), vec![model_a_info]);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        Some(900),
        "unresolved model B must survive provider A discovery"
    );

    let mut model_b_info = base_info;
    model_b_info.id = model_b.clone();
    model_b_info.supports_compaction = false;
    model_b_info.supports_standalone_compaction = true;
    model_b_info.standalone_compaction_threshold = Some(900);
    h.set_provider_models(&crate::test_connection_id("provider-b"), vec![model_b_info]);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        Some(900)
    );
    assert_eq!(
        h.model_for_agent_role(&h.agent_runtime.agent_registry.agents[&cid]),
        Some(model_b.clone())
    );

    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::user("first activation after provider B".to_owned()),
    )
    .expect("dispatch first activation");
    assert_eq!(
        read_nth_prompt_created(&h, 0).operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
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
        Some(900)
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
/// defaults, then retain explicit saturated endpoints through the dispatch
/// path.
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
    settings.efforts = vec![
        tau_proto::Effort::Off,
        tau_proto::Effort::Low,
        tau_proto::Effort::Medium,
        tau_proto::Effort::High,
        tau_proto::Effort::Max,
    ];
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
            adjustment: tau_proto::UiRoleSettingAdjustment::Increase(one),
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
    assert_eq!(updated_role.effort, Some(tau_proto::Effort::High));
    assert_eq!(updated_role.verbosity, Some(tau_proto::Verbosity::Medium));
    assert_eq!(
        updated_role.thinking_summary,
        Some(tau_proto::ThinkingSummary::Concise)
    );

    let far = NonZeroU8::new(99).expect("positive adjustment");
    for action in [
        tau_proto::UiRoleUpdateAction::AdjustEffort {
            adjustment: tau_proto::UiRoleSettingAdjustment::Decrease(far),
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
    assert_eq!(updated_role.effort, Some(tau_proto::Effort::Off));
    assert_eq!(updated_role.verbosity, Some(tau_proto::Verbosity::Low));
    assert_eq!(
        updated_role.thinking_summary,
        Some(tau_proto::ThinkingSummary::Off)
    );

    for action in [
        tau_proto::UiRoleUpdateAction::AdjustEffort {
            adjustment: tau_proto::UiRoleSettingAdjustment::Increase(far),
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
    assert_eq!(updated_role.effort, Some(tau_proto::Effort::Max));
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
    settings.efforts = vec![tau_proto::Effort::Medium, tau_proto::Effort::High];
    settings.verbosities = vec![tau_proto::Verbosity::Low, tau_proto::Verbosity::Medium];
    settings.thinking_summaries = vec![
        tau_proto::ThinkingSummary::Off,
        tau_proto::ThinkingSummary::Auto,
        tau_proto::ThinkingSummary::Concise,
    ];
    for action in [
        tau_proto::UiRoleUpdateAction::AdjustEffort {
            adjustment: tau_proto::UiRoleSettingAdjustment::Decrease(one),
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
    assert_eq!(updated_role.effort, Some(tau_proto::Effort::Medium));
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
    settings.efforts = vec![
        tau_proto::Effort::Off,
        tau_proto::Effort::Low,
        tau_proto::Effort::Medium,
        tau_proto::Effort::High,
        tau_proto::Effort::Max,
    ];
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
    assert_eq!(h.selected_model_params().effort, tau_proto::Effort::Medium);
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
        conv.execution.context_input_tokens = Some(800);
        conv.execution.context_usage_head = conv.identity.head;
        conv.execution.context_usage_model = Some(alternate.clone());
        conv.execution.context_cached_tokens = Some(400);
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
        conv.execution.context_input_tokens = Some(700);
        conv.execution.context_usage_head = conv.identity.head;
        conv.execution.context_usage_model = Some("test/model".into());
        conv.execution.context_cached_tokens = Some(350);
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
        Some(700)
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
        Some(10)
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_cached_tokens,
        Some(10)
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
        compaction_compacted_input_tokens: None,
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
        compaction_compacted_input_tokens: None,
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
        .effort = Some(tau_proto::Effort::Low);

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
        compaction_compacted_input_tokens: None,
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
        .effort = Some(tau_proto::Effort::High);

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
        compaction_compacted_input_tokens: None,
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
