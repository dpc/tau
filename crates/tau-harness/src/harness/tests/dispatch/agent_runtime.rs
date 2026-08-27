//! Tests for agent runtime behavior.

use super::*;

/// Repeated ensure calls for an already-loaded agent are a hot-path no-op and
/// must not deserialize historical journals again.
#[test]
fn repeated_loaded_agent_ensure_does_not_rescan_history() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let mut h = quiet_provider_harness(&state_dir).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let notice_count = h.runtime_io.replayable_harness_notices.len();

    let journal = state_dir
        .join("agents")
        .join(agent_id.as_str())
        .join("events.cbor");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !journal.exists() {
        assert!(
            Instant::now() < deadline,
            "accepted creation was not written"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    std::fs::write(journal, b"corrupt after initial load").expect("corrupt journal");
    h.ensure_loaded_agent_for_agent(&cid, agent_id.as_str());

    assert_eq!(h.runtime_io.replayable_harness_notices.len(), notice_count);
    h.shutdown().expect("shutdown");
}

/// Terminal teardown keeps routes until unload commits but must reject every
/// direct message and prompt entry point during that interval.
#[test]
fn terminating_agent_route_rejects_direct_work() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .terminating = true;

    assert_ne!(
        h.agent_message_recipient_status(&recipient_id),
        crate::harness::AgentMessageRecipientStatus::Live
    );
    h.activate_received_agent_message(
        &tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("msg-during-termination")
                .expect("test identifier must satisfy its grammar"),
            sender_id: tau_proto::AgentId::parse("sender").expect("agent id"),
            sender_session_id: None,
            recipient_id: tau_proto::AgentId::parse(&recipient_id).expect("agent id"),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "must be rejected".to_owned(),
        },
        Some(&tau_core::AgentAppendOutcome {
            observation_id: tau_proto::ObservationId::from_bytes([1; 16]),
            seq: tau_core::PersistedAgentEventSeq::new(1),
            folded_node_id: None,
            selected_head_id: None,
        }),
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .is_empty()
    );
    assert!(
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("rejected".to_owned()))
            .is_err()
    );
    assert!(matches!(
        h.submit_prompt_to_agent(
            test_session_id("s1"),
            &recipient_id,
            PendingPrompt::user("also rejected".to_owned())
        )
        .expect("rejection is in-band"),
        crate::harness::PromptSubmission::Rejected { .. }
    ));
}

/// UI and extension peers cannot author canonical transcript facts or their
/// harness-owned activation bits through direct or generic-emit intake.
#[test]
fn inbound_canonical_activation_forgery_is_ignored() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = echo_harness(&state).expect("start");
    let agent_id = crate::parse_agent_id("forged-agent");
    let forged = [
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: true,
            agent_id: agent_id.clone(),
            text: "forged submitted".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HumanUi,
            display_name: None,
            ctx_id: None,
        }),
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            inference_activation: true,
            agent_id: agent_id.clone(),
            text: "forged injected".to_owned(),
            message_class: tau_proto::PromptMessageClass::Internal,
        }),
        Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
            self_compaction_terminal: None,
            inference_activation: true,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            agent_id,
            text: "forged steered".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            ctx_id: None,
        }),
    ];

    for event in forged {
        let baseline_seq = h.runtime_io.event_log.next_seq();
        h.handle_client_event_inner(&crate::test_connection_id("ui"), event.clone())
            .expect("ui event ignored");
        h.handle_extension_event_inner(&crate::test_connection_id("extension"), event.clone())
            .expect("extension event ignored");
        h.handle_extension_message(
            &crate::test_connection_id("extension"),
            TestMessage::Emit(tau_proto::Emit {
                event: Box::new(event),
                persist: true,
            }),
        )
        .expect("extension emit ignored");
        assert!(h.runtime_io.event_log.get_next_from(baseline_seq).is_none());
    }
    assert!(
        h.session_runtime
            .agent_store
            .agent_events("forged-agent")
            .expect("agent events")
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// Supplied handlers cannot replace the intrinsic implementation or smuggle
/// additional registrations through a handler that claims its reserved name.
#[test]
fn self_info_reserved_handler_claim_is_excluded() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.install_internal_tool_handlers(vec![path_std_sync::Arc::new(ReservedSelfInfoClaim)]);

    assert_eq!(h.tool_routing.internal_tool_handlers.len(), 1);
    assert!(h.tool_routing.internal_tool_handlers[0].handles(&ToolName::new("self_info")));
    let self_info_specs = h
        .tool_routing
        .registry
        .all_tools()
        .into_iter()
        .filter(|tool| tool.name.as_str() == "self_info")
        .collect::<Vec<_>>();
    assert_eq!(self_info_specs.len(), 1);
    assert!(self_info_specs[0].enabled_by_default);
    assert_eq!(
        self_info_specs[0].parameters,
        Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }))
    );
    assert!(
        !h.tool_routing
            .registry
            .all_tools()
            .iter()
            .any(|tool| { matches!(tool.name.as_str(), "must_not_register") })
    );
    h.shutdown().expect("shutdown");
}

/// A selected obligation that remains queued after preflight must be attempted
/// at most once per drain invocation and must not block a later runnable agent.
#[test]
fn retained_unroutable_dispatch_does_not_head_of_line_block_other_agent() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let unroutable_cid = ensure_test_user_agent(&mut h);
    let runnable_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let unroutable_agent_id = durable_agent_id_for_conversation(&h, &unroutable_cid);
    let runnable_agent_id = durable_agent_id_for_conversation(&h, &runnable_cid);
    h.config.available_roles.insert(
        "unroutable-test-role".to_owned(),
        tau_config::settings::AgentRole {
            model: Some("missing/model".into()),
            ..Default::default()
        },
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&unroutable_cid)
        .expect("unroutable agent")
        .identity
        .role = Some("unroutable-test-role".to_owned());
    let context_provider = tau_proto::ConnectionId::parse("unroutable-fairness-context")
        .expect("test connection id must satisfy the identifier grammar");
    for agent_id in [&unroutable_agent_id, &runnable_agent_id] {
        set_test_agent_context_wait(
            &mut h,
            agent_id.clone(),
            path_std_collections::HashSet::from([context_provider.clone()]),
        );
    }
    h.dispatch_prompt_for_agent(
        &unroutable_cid,
        PendingPrompt::user("unroutable activation".to_owned()),
    )
    .expect("defer unroutable activation");
    h.dispatch_prompt_for_agent(
        &runnable_cid,
        PendingPrompt::user("runnable activation".to_owned()),
    )
    .expect("defer runnable activation");
    finish_test_agent_context_wait(&mut h, &unroutable_agent_id);
    finish_test_agent_context_wait(&mut h, &runnable_agent_id);
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 2);
    let unroutable_obligation = h.runtime_io.publication.idle_dispatches[0].clone();

    h.drain_publish_idle_dispatches();

    let runnable_prompt = event_log_events(&h)
        .iter()
        .find_map(|event| match event {
            Event::AgentPromptCreated(created) if created.agent_id == runnable_agent_id => {
                Some(created.clone())
            }
            _ => None,
        })
        .expect("later runnable agent prompt");
    let provider = h.provider_runtime.pending_prompts[&runnable_prompt.agent_prompt_id].clone();
    h.handle_extension_event_inner(
        &provider,
        Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: runnable_prompt.agent_prompt_id.clone(),
            originator: runnable_prompt.originator.clone(),
        }),
    )
    .expect("record runnable provider submission");
    let runnable_sequence = event_log_events(&h)
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == runnable_prompt.agent_prompt_id =>
            {
                Some("checkpoint")
            }
            Event::AgentPromptStarted(started)
                if started.agent_prompt_id == runnable_prompt.agent_prompt_id =>
            {
                Some("started")
            }
            Event::AgentPromptCreated(created)
                if created.agent_prompt_id == runnable_prompt.agent_prompt_id =>
            {
                Some("created")
            }
            Event::ProviderPromptSubmitted(submitted)
                if submitted.agent_prompt_id == runnable_prompt.agent_prompt_id =>
            {
                Some("submitted")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        runnable_sequence,
        ["checkpoint", "started", "created", "submitted"]
    );
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);
    let retained = &h.runtime_io.publication.idle_dispatches[0];
    assert_eq!(retained.cid, unroutable_obligation.cid);
    assert_eq!(
        retained.activation_cut,
        unroutable_obligation.activation_cut
    );
    assert_eq!(
        retained.activation_through,
        unroutable_obligation.activation_through
    );
    assert_eq!(
        retained.obligation.is_committed(),
        unroutable_obligation.obligation.is_committed()
    );
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentInferenceDispatchStarted(started)
            if started.agent_id == unroutable_agent_id
    )));

    h.drain_publish_idle_dispatches();
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentInferenceDispatchStarted(started)
            if started.agent_id == unroutable_agent_id
    )));

    h.config
        .available_roles
        .get_mut("unroutable-test-role")
        .expect("test role")
        .model = Some("test/model".into());
    h.drain_publish_idle_dispatches();
    let events = event_log_events(&h);
    let unroutable_checkpoints = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_id == unroutable_agent_id =>
            {
                Some(started)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(unroutable_checkpoints.len(), 1);
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_prompt_id == unroutable_checkpoints[0].agent_prompt_id
                && created.agent_id == unroutable_agent_id
    )));
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    h.shutdown().expect("shutdown");
}

#[test]
fn delegated_agent_user_interaction_prevents_auto_suspend() {
    // If a UI targets a running delegated agent before its delegated reply is
    // returned, that interaction converts it into a normal active agent. The
    // later delegate completion must not hide it from `:agent switch`.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    let parent = ensure_test_user_agent(&mut h);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("delegate-call".into(), parent);

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-user".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");
    let side_cid = ext_query_cid(&h, "q-user").expect("side conversation");
    let side_agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&side_cid)
        .and_then(|conv| conv.identity.agent_id.clone())
        .expect("side agent id");

    h.submit_prompt_to_agent(
        test_session_id("s1"),
        &side_agent_id,
        "user follow-up".to_owned(),
    )
    .expect("user prompt to delegate");

    let side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "delegated answer".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("conn-delegate"),
            query_id: "q-user".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side finished");

    assert!(
        h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(&side_agent_id),
        "interacted delegate remains targetable"
    );

    h.shutdown().expect("shutdown");
}

/// Delegated agents are durable, user-addressable agents, not temporary
/// implementation details. Their harness conversation id should therefore be
/// the minted public agent id instead of the old deterministic
/// `start-agent-{extension}-{query}` key.
#[test]
fn start_agent_request_conversation_id_is_public_agent_id() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    for conn_id in ["conn-delegate-a", "conn-delegate-b"] {
        let connection_id: tau_proto::ConnectionId = crate::test_connection_id(conn_id);
        h.extensions.entries.insert(
            connection_id.clone(),
            crate::extension::ExtensionEntry {
                tool_prefix: None,
                name: crate::test_extension_name("delegate-ext"),
                instance_id: 42.into(),
                connection_id: connection_id.clone(),
                kind: tau_proto::ClientKind::Tool,
                peer_capabilities: Default::default(),
                require: true,
                respawn_allowed: true,
                pid: None,
                in_process_thread: None,
                supervised_config: None,
                secrets: path_std_collections::BTreeMap::new(),
                restart_attempt: 0,
                state: path_crate_extension::ExtensionState::Ready,
                protocol_io: tau_client::ProtocolIoMeter::default(),
            },
        );
        h.extensions.order.push(connection_id);
    }
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate-a"),
        ext_query("q-named"),
    )
    .expect("query");
    let mut side_agents = h
        .agent_runtime
        .agent_registry
        .agents
        .iter()
        .filter(|(_, conv)| {
            matches!(
                &conv.identity.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. } if query_id == "q-named"
            )
        });
    let (cid, conv) = side_agents.next().expect("side agent");
    assert!(side_agents.next().is_none());
    let public_agent_id = conv.identity.agent_id.as_deref().expect("public agent id");
    assert_eq!(cid.as_str(), public_agent_id);
    assert!(!cid.as_str().starts_with("start-agent-"));
    let cid = cid.clone();
    let agent_count = h.agent_runtime.agent_registry.agents.len();
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate-b"),
        ext_query("q-named"),
    )
    .expect("duplicate query");
    assert_eq!(h.agent_runtime.agent_registry.agents.len(), agent_count);
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .and_then(|conv| conv.identity.source_connection.as_deref()),
        Some("conn-delegate-b")
    );
    h.shutdown().expect("shutdown");
}
