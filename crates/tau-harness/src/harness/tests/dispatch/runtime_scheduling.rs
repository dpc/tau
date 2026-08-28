//! Tests for runtime scheduling behavior.

use super::*;

/// A physical post-accept creation failure terminalizes accepted requests
/// without exposing the failed identity while later FIFO work continues.
#[test]
fn accepted_start_storage_failure_terminalizes_and_continues_fifo() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("harness");
    let first = h
        .prepare_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query("q-fail"),
        )
        .expect("prepare first")
        .expect("first pending");
    let second = h
        .prepare_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query("q-next"),
        )
        .expect("prepare second")
        .expect("second pending");
    let first_agent_id = first.agent_id.clone();
    let second_agent_id = second.agent_id.clone();
    h.accept_duplicate_start_agent_request(
        &crate::test_connection_id(HARNESS_CONNECTION_ID),
        &first.query.query_id,
        &first_agent_id,
    );
    h.accept_duplicate_start_agent_request(
        &crate::test_connection_id(HARNESS_CONNECTION_ID),
        &second.query.query_id,
        &second_agent_id,
    );
    h.agent_runtime
        .agent_registry
        .pending_start_requests
        .push_back(first);
    h.agent_runtime
        .agent_registry
        .pending_start_requests
        .push_back(second);
    let blocked_journal = state
        .join("agents")
        .join(&first_agent_id)
        .join("events.cbor");
    std::fs::create_dir_all(&blocked_journal).expect("block first journal");

    h.drain_pending_start_agent_requests().expect("drain");

    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::StartAgentAccepted(accepted) if accepted.query_id == "q-fail"
            ))
            .count(),
        1
    );
    let failed_results: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::StartAgentResult(result) if result.query_id == "q-fail" => Some(result),
            _ => None,
        })
        .collect();
    assert!(
        failed_results.is_empty(),
        "asynchronous collision cannot retract accepted creation"
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(&first_agent_id)
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&crate::parse_agent_id(&first_agent_id))
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .session_loaded
            .contains(&crate::parse_agent_id(&first_agent_id))
    );
    assert!(
        h.session_runtime
            .store
            .session("s1")
            .is_some_and(|membership| {
                membership.contains_agent(&crate::parse_agent_id(&first_agent_id))
            })
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentPromptSubmitted(prompt) if prompt.agent_id.as_str() == first_agent_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentStarted(started) if started.agent_id.as_str() == first_agent_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::SessionAgentLoaded(loaded) if loaded.agent_id.as_str() == first_agent_id
    )));
    assert!(
        h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(&second_agent_id)
    );
    let second_records = h
        .session_runtime
        .agent_store
        .agent_events(&second_agent_id)
        .expect("second agent records");
    assert!(!second_records.is_empty());
    assert!(
        h.session_runtime
            .store
            .session("s1")
            .is_some_and(|membership| {
                membership.contains_agent(&crate::parse_agent_id(&second_agent_id))
            })
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::StartAgentResult(result)
                    if result.query_id == "q-next" && result.error.is_none()
            ))
            .count(),
        0
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentStarted(started) if started.agent_id.as_str() == second_agent_id
    )));
    assert!(
        h.agent_runtime
            .agent_registry
            .pending_start_requests
            .is_empty()
    );
}

#[test]
fn shared_start_agent_requests_start_concurrently() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-a");
    let _ = connect_test_tool(&mut h, "conn-b");

    h.handle_start_agent_request(&crate::test_connection_id("conn-a"), ext_query("q-a"))
        .expect("query a");
    h.handle_start_agent_request(&crate::test_connection_id("conn-b"), ext_query("q-b"))
        .expect("query b");

    assert!(ext_query_cid(&h, "q-a").is_some());
    assert!(ext_query_cid(&h, "q-b").is_some());
    assert!(
        h.agent_runtime
            .agent_registry
            .pending_start_requests
            .is_empty()
    );

    h.shutdown().expect("shutdown");
}

/// Start-agent requests do not use harness-level scheduling; filesystem
/// coordination is handled by ext-shell directory locks.
#[test]
fn start_agent_requests_do_not_block_independent_queries() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-a");
    let _ = connect_test_tool(&mut h, "conn-b");
    let _ = connect_test_tool(&mut h, "conn-c");
    let _ = connect_test_tool(&mut h, "conn-d");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-a"),
        ext_query("q-update-a"),
    )
    .expect("update query a");
    h.handle_start_agent_request(&crate::test_connection_id("conn-b"), ext_query("q-shared"))
        .expect("shared query");
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-c"),
        ext_query("q-update-b"),
    )
    .expect("update query b");
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-d"),
        ext_query("q-exclusive"),
    )
    .expect("exclusive query");

    for query_id in ["q-update-a", "q-shared", "q-update-b", "q-exclusive"] {
        assert!(
            ext_query_cid(&h, query_id).is_some(),
            "{query_id} should start immediately"
        );
    }
    assert!(
        h.agent_runtime
            .agent_registry
            .pending_start_requests
            .is_empty()
    );

    h.shutdown().expect("shutdown");
}

/// Tool-backed nested start-agent requests are independent agents and do not
/// wait on their parent at harness level.
#[test]
fn nested_start_agent_request_starts_independently() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        ext_query("q-outer"),
    )
    .expect("outer query");
    let outer_cid = ext_query_cid(&h, "q-outer").expect("outer started");

    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("nested-call".into(), outer_cid.clone());
    let mut nested = ext_query("q-nested");
    nested.tool_call_id = Some("nested-call".into());
    nested.task_name = Some("nested".to_owned());
    h.handle_start_agent_request(&crate::test_connection_id("conn-delegate"), nested)
        .expect("nested query");

    let nested_cid = ext_query_cid(&h, "q-nested").expect("nested started");
    assert_ne!(outer_cid, nested_cid);
    assert!(
        h.agent_runtime
            .agent_registry
            .pending_start_requests
            .is_empty()
    );

    h.shutdown().expect("shutdown");
}

/// Releasing a later agent's context barrier must bypass an earlier retained
/// context-not-ready obligation without consuming, duplicating, or rebinding
/// it. This protects per-agent readiness independence and bounded no-progress
/// drains.
#[test]
fn reverse_agent_context_readiness_dispatches_each_obligation_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let retained_cid = ensure_test_user_agent(&mut h);
    let ready_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let retained_agent_id = durable_agent_id_for_conversation(&h, &retained_cid);
    let ready_agent_id = durable_agent_id_for_conversation(&h, &ready_cid);
    let context_provider = tau_proto::ConnectionId::parse("reverse-readiness-context")
        .expect("test connection id must satisfy the identifier grammar");
    for agent_id in [&retained_agent_id, &ready_agent_id] {
        set_test_agent_context_wait(
            &mut h,
            agent_id.clone(),
            path_std_collections::HashSet::from([context_provider.clone()]),
        );
    }

    h.dispatch_prompt_for_agent(
        &retained_cid,
        PendingPrompt::user("retained activation".to_owned()),
    )
    .expect("defer retained activation");
    h.dispatch_prompt_for_agent(
        &ready_cid,
        PendingPrompt::user("ready activation".to_owned()),
    )
    .expect("defer ready activation");
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 2);
    assert_eq!(
        h.runtime_io.publication.idle_dispatches[0].cid,
        retained_cid
    );
    assert_eq!(h.runtime_io.publication.idle_dispatches[1].cid, ready_cid);
    let retained_obligation = h.runtime_io.publication.idle_dispatches[0].clone();
    let ready_obligation = h.runtime_io.publication.idle_dispatches[1].clone();
    assert!(retained_obligation.obligation.is_committed());
    assert!(ready_obligation.obligation.is_committed());
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&retained_cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&ready_cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));

    finish_test_agent_context_wait(&mut h, &ready_agent_id);
    h.drain_publish_idle_dispatches();

    let events = event_log_events(&h);
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentInferenceDispatchStarted(started)
            if started.agent_id == retained_agent_id
    )));
    let ready_checkpoint = assert_inference_dispatch_lifecycle(
        &events,
        &ready_agent_id,
        ready_obligation
            .activation_through
            .expect("ready activation watermark"),
        ready_obligation.activation_cut,
        ExpectedProviderSubmission::Pending,
    );
    assert_inference_dispatch_owner(
        &h.agent_runtime.agent_registry.agents[&ready_cid],
        &ready_checkpoint,
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&retained_cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);
    let retained = &h.runtime_io.publication.idle_dispatches[0];
    assert_eq!(retained.cid, retained_cid);
    assert_eq!(
        retained.activation_through,
        retained_obligation.activation_through
    );
    assert_eq!(retained.activation_cut, retained_obligation.activation_cut);
    assert_eq!(
        retained.obligation.is_committed(),
        retained_obligation.obligation.is_committed()
    );

    let events_before_stable_drain = event_log_events(&h);
    h.drain_publish_idle_dispatches();
    assert_eq!(event_log_events(&h), events_before_stable_drain);
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);
    let retained = &h.runtime_io.publication.idle_dispatches[0];
    assert_eq!(retained.cid, retained_cid);
    assert_eq!(
        retained.activation_through,
        retained_obligation.activation_through
    );
    assert_eq!(retained.activation_cut, retained_obligation.activation_cut);
    assert_eq!(
        retained.obligation.is_committed(),
        retained_obligation.obligation.is_committed()
    );

    let ready_provider =
        h.provider_runtime.pending_prompts[&ready_checkpoint.agent_prompt_id].clone();
    h.handle_extension_event_inner(
        &ready_provider,
        Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: ready_checkpoint.agent_prompt_id.clone(),
            originator: tau_proto::PromptOriginator::User,
        }),
    )
    .expect("record ready-agent provider submission");
    let events = event_log_events(&h);
    let submitted_ready_checkpoint = assert_inference_dispatch_lifecycle(
        &events,
        &ready_agent_id,
        ready_checkpoint.through,
        ready_checkpoint.activation_cut,
        ExpectedProviderSubmission::Submitted,
    );
    assert_eq!(
        submitted_ready_checkpoint.agent_prompt_id,
        ready_checkpoint.agent_prompt_id
    );
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);
    assert_eq!(
        h.runtime_io.publication.idle_dispatches[0].cid,
        retained_cid
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentInferenceDispatchStarted(started)
            if started.agent_id == retained_agent_id
    )));

    finish_test_agent_context_wait(&mut h, &retained_agent_id);
    h.drain_publish_idle_dispatches();

    let events = event_log_events(&h);
    let retained_checkpoint = assert_inference_dispatch_lifecycle(
        &events,
        &retained_agent_id,
        retained_obligation
            .activation_through
            .expect("retained activation watermark"),
        retained_obligation.activation_cut,
        ExpectedProviderSubmission::Pending,
    );
    assert_inference_dispatch_owner(
        &h.agent_runtime.agent_registry.agents[&retained_cid],
        &retained_checkpoint,
    );
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());

    let retained_provider =
        h.provider_runtime.pending_prompts[&retained_checkpoint.agent_prompt_id].clone();
    h.handle_extension_event_inner(
        &retained_provider,
        Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: retained_checkpoint.agent_prompt_id.clone(),
            originator: tau_proto::PromptOriginator::User,
        }),
    )
    .expect("record retained-agent provider submission");
    h.drain_publish_idle_dispatches();
    let events = event_log_events(&h);
    for (agent_id, checkpoint) in [
        (&retained_agent_id, &retained_checkpoint),
        (&ready_agent_id, &ready_checkpoint),
    ] {
        let submitted_checkpoint = assert_inference_dispatch_lifecycle(
            &events,
            agent_id,
            checkpoint.through,
            checkpoint.activation_cut,
            ExpectedProviderSubmission::Submitted,
        );
        assert_eq!(
            submitted_checkpoint.agent_prompt_id,
            checkpoint.agent_prompt_id
        );
    }
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    h.shutdown().expect("shutdown");
}

/// One selected deferred obligation with uncertain ownership must not become a
/// global agent slot that blocks a later runnable agent.
#[test]
fn blocked_deferred_dispatch_does_not_head_of_line_block_other_agent() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let blocked_cid = ensure_test_user_agent(&mut h);
    let runnable_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let blocked_agent_id = durable_agent_id_for_conversation(&h, &blocked_cid);
    let runnable_agent_id = durable_agent_id_for_conversation(&h, &runnable_cid);
    let context_provider = tau_proto::ConnectionId::parse("deferred-fairness-context")
        .expect("test connection id must satisfy the identifier grammar");
    for agent_id in [&blocked_agent_id, &runnable_agent_id] {
        set_test_agent_context_wait(
            &mut h,
            agent_id.clone(),
            path_std_collections::HashSet::from([context_provider.clone()]),
        );
    }

    h.dispatch_prompt_for_agent(
        &blocked_cid,
        PendingPrompt::user("blocked activation".to_owned()),
    )
    .expect("defer blocked activation");
    h.dispatch_prompt_for_agent(
        &runnable_cid,
        PendingPrompt::user("runnable activation".to_owned()),
    )
    .expect("defer runnable activation");
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 2);
    assert_eq!(h.runtime_io.publication.idle_dispatches[0].cid, blocked_cid);
    assert_eq!(
        h.runtime_io.publication.idle_dispatches[1].cid,
        runnable_cid
    );
    let blocked_obligation = h.runtime_io.publication.idle_dispatches[0].clone();
    let runnable_obligation = h.runtime_io.publication.idle_dispatches[1].clone();
    finish_test_agent_context_wait(&mut h, &blocked_agent_id);
    finish_test_agent_context_wait(&mut h, &runnable_agent_id);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&blocked_cid)
        .expect("blocked agent")
        .dispatch
        .activation_dispatch = path_crate_agent::ActivationDispatchState::DispatchUncertain {
        owner: path_crate_agent::InferenceCheckpointOwner::Inference,
        agent_prompt_id: test_agent_prompt_id("ap-blocked-uncertain"),
        through: blocked_obligation
            .activation_through
            .expect("blocked activation watermark"),
        model: Some("test/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: blocked_obligation.activation_cut,
    };

    h.drain_publish_idle_dispatches();

    let events = event_log_events(&h);
    assert!(
        events.iter().all(
            |event| !matches!(event, Event::AgentInferenceDispatchStarted(started)
                if started.agent_id == blocked_agent_id)
        ),
        "blocked agent must not receive another checkpoint"
    );
    let runnable_checkpoints = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_id == runnable_agent_id =>
            {
                Some(started)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(runnable_checkpoints.len(), 1);
    let runnable_checkpoint = runnable_checkpoints[0];
    assert_eq!(
        runnable_checkpoint.through,
        runnable_obligation
            .activation_through
            .expect("runnable activation watermark")
    );
    assert_eq!(
        runnable_checkpoint.activation_cut,
        runnable_obligation.activation_cut
    );
    assert_eq!(runnable_checkpoint.model, Some("test/model".into()));
    assert_eq!(
        runnable_checkpoint.operation,
        Some(tau_proto::PromptOperation::Inference)
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentPromptStarted(started)
            if started.agent_prompt_id == runnable_checkpoint.agent_prompt_id
                && started.agent_id == runnable_agent_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_prompt_id == runnable_checkpoint.agent_prompt_id
                && created.agent_id == runnable_agent_id
    )));
    assert_eq!(
        h.runtime_io
            .publication
            .idle_dispatches
            .iter()
            .filter(|deferred| deferred.cid == blocked_cid)
            .count(),
        1
    );
    assert!(
        h.runtime_io
            .publication
            .idle_dispatches
            .iter()
            .all(|deferred| deferred.cid != runnable_cid)
    );

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&blocked_cid)
        .expect("release blocked agent")
        .dispatch
        .activation_dispatch = path_crate_agent::ActivationDispatchState::None;
    h.drain_publish_idle_dispatches();

    let events = event_log_events(&h);
    let blocked_checkpoints = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_id == blocked_agent_id =>
            {
                Some(started)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(blocked_checkpoints.len(), 1);
    assert_eq!(
        blocked_checkpoints[0].through,
        blocked_obligation
            .activation_through
            .expect("blocked activation watermark")
    );
    assert_eq!(
        blocked_checkpoints[0].activation_cut,
        blocked_obligation.activation_cut
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_prompt_id == blocked_checkpoints[0].agent_prompt_id
                && created.agent_id == blocked_agent_id
    )));
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    h.shutdown().expect("shutdown");
}

/// Readiness coalescing keeps incomparable branch activations as distinct
/// obligations; dispatching the selected sibling does not consume the dormant
/// branch, which becomes runnable after the sibling turn finishes and
/// reselects.
#[test]
fn readiness_deferred_incomparable_activations_remain_distinct() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    set_test_agent_context_wait(
        &mut h,
        agent_id.clone(),
        path_std_collections::HashSet::from([tau_proto::ConnectionId::parse("context-provider")
            .expect("test connection id must satisfy the identifier grammar")]),
    );

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("branch A activation".to_owned()))
        .expect("park branch A");
    let branch_a = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("branch A");
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("branch B activation".to_owned()))
        .expect("park branch B");
    let branch_b = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("branch B");
    assert_ne!(branch_a, branch_b);
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 2);

    finish_test_agent_context_wait(&mut h, &agent_id);
    h.drain_publish_idle_dispatches();
    let branch_b_prompt = read_nth_prompt_created(&h, 0);
    let checkpoints = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].through, tau_proto::AgentHead::Node(branch_b));
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);

    h.handle_provider_response_finished(provider_text_response(
        &branch_b_prompt.agent_prompt_id,
        agent_id.clone(),
        "branch B complete",
    ))
    .expect("finish selected sibling");
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: tau_proto::AgentHead::Node(branch_a),
        }),
    );
    let checkpoints = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(
        checkpoints[1].through,
        tau_proto::AgentHead::Node(branch_a),
        "branch_a={branch_a:?}, branch_b={branch_b:?}, checkpoints={checkpoints:?}"
    );
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
}

/// Initial and post-tool normal/canceled terminals each own one durable outer
/// finish and protected automatic-compaction start, with no retained
/// completion.
#[test]
fn automatic_policy_terminal_matrix_commits_owned_suffix_once() {
    for post_tool in [false, true] {
        for canceled in [false, true] {
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
            h.config
                .available_roles
                .get_mut(&h.config.selected_role)
                .expect("selected role")
                .compactions
                .insert(
                    "owned-terminal-matrix".to_owned(),
                    tau_config::settings::CompactionPolicy {
                        threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(1),
                        enable: true,
                        when: tau_config::settings::ContextPolicyWhen {
                            at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
                            statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
                        },
                    },
                );
            {
                let agent = h
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get_mut(&cid)
                    .expect("agent");
                agent.execution.context_input_tokens = Some(100);
                agent.execution.context_usage_model = Some("test/model".into());
                agent.execution.context_usage_prompt_id =
                    Some(test_agent_prompt_id("ap-test-provider-usage"));
                agent.execution.context_usage_head = agent.identity.head;
            }
            h.dispatch_prompt_for_agent(
                &cid,
                PendingPrompt::user(format!("matrix post_tool={post_tool} canceled={canceled}")),
            )
            .expect("dispatch initial inference");
            let initial = read_nth_prompt_created(&h, 0);
            let terminal_prompt = if post_tool {
                h.handle_provider_response_finished(provider_tool_response(
                    &initial,
                    "matrix-tool",
                    "self_info",
                    CborValue::Map(Vec::new()),
                ))
                .expect("finish matrix tool round");
                read_nth_prompt_created(&h, 1)
            } else {
                initial
            };
            if canceled {
                h.finalize_canceled_in_flight_prompt(&cid);
            } else {
                let mut response = provider_text_response(
                    &terminal_prompt.agent_prompt_id,
                    terminal_prompt.agent_id.clone(),
                    "done",
                );
                response.usage = Some(tau_proto::ProviderTokenUsage {
                    prompt_sent_tokens: 250,
                    response_received_tokens: 1,
                    ..Default::default()
                });
                h.handle_provider_response_finished(response)
                    .expect("finish matrix inference");
            }

            let records = h
                .session_runtime
                .agent_store
                .agent_events(terminal_prompt.agent_id.as_str())
                .expect("durable records");
            let terminals = records
                .iter()
                .filter_map(|record| match &record.event {
                    Event::ProviderResponseFinished(response)
                        if response.agent_prompt_id == terminal_prompt.agent_prompt_id =>
                    {
                        Some(&record.event)
                    }
                    Event::AgentPromptTerminated(terminated)
                        if terminated.agent_prompt_id == terminal_prompt.agent_prompt_id =>
                    {
                        Some(&record.event)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                terminals.len(),
                1,
                "post_tool={post_tool} canceled={canceled}"
            );
            if canceled {
                let Event::AgentPromptTerminated(terminated) = terminals[0] else {
                    panic!("canceled matrix terminal");
                };
                assert!(
                    terminated.automatic_compaction_decision.is_none(),
                    "cancellation without a prior exact provider observation cannot mint authority"
                );
                assert!(records.iter().all(|record| !matches!(
                    record.event,
                    Event::AgentStandaloneCompactionStarted(_)
                )));
                h.shutdown().expect("shutdown");
                continue;
            }
            let decision = match terminals[0] {
                Event::ProviderResponseFinished(response) if !canceled => response
                    .automatic_compaction_decision
                    .as_ref()
                    .expect("ordinary terminal owns decision"),
                Event::AgentPromptTerminated(terminated) if canceled => terminated
                    .automatic_compaction_decision
                    .as_ref()
                    .expect("canceled terminal owns decision"),
                Event::ProviderResponseFinished(_) | Event::AgentPromptTerminated(_) => {
                    panic!(
                        "terminal variant disagrees with post_tool={post_tool} canceled={canceled}"
                    )
                }
                _ => unreachable!("filtered canonical terminal"),
            };
            let transaction_id = &decision.transaction_id;
            let finishes = records
                .iter()
                .filter_map(|record| match &record.event {
                    Event::AgentOuterTurnFinished(finished)
                        if finished.outer_turn_id == decision.outer_turn_id =>
                    {
                        Some(finished)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                finishes.len(),
                1,
                "post_tool={post_tool} canceled={canceled}"
            );
            assert_eq!(
                finishes[0].automatic_compaction_decision.as_ref(),
                Some(transaction_id),
                "post_tool={post_tool} canceled={canceled}"
            );
            assert_eq!(
                records
                    .iter()
                    .filter(|record| matches!(
                        &record.event,
                        Event::AgentStandaloneCompactionStarted(started)
                            if &started.transaction_id == transaction_id
                    ))
                    .count(),
                1,
                "post_tool={post_tool} canceled={canceled}"
            );
            assert!(
                h.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .is_empty(),
                "post_tool={post_tool} canceled={canceled}"
            );
            h.shutdown().expect("shutdown");
        }
    }
}

/// Outer-finish notices consume the exact terminal candidate, ignore live-role
/// rewrites, and preserve one-shot hysteresis while usage remains high.
#[test]
fn outer_finish_alert_uses_terminal_snapshot_and_retains_hysteresis() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let prompt_id = tau_proto::AgentPromptId::parse("ap-alert-finish").expect("prompt");
    let outer_turn_id = tau_proto::AgentOuterTurnId::for_prompt(&prompt_id);
    let alert = tau_config::settings::ContextSizeAlert {
        threshold: path_tau_config_settings::ContextSizeAlertThreshold::new(100)
            .expect("positive test threshold"),
        enable: true,
        message: "captured finish alert".to_owned(),
        when: tau_config::settings::ContextPolicyWhen {
            at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
            statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
        },
    };
    let arm = |agent: &mut Agent| {
        agent.execution.context_input_tokens = Some(200);
        agent.turn.terminal_status_was_available = false;
        agent.turn.terminal_notice_eligible = true;
        agent.turn.terminal_notice_outer_turn_id = Some(outer_turn_id.clone());
        agent
            .turn
            .terminal_context_size_alerts
            .insert("captured".to_owned(), alert.clone());
    };
    arm(h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent"));
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("role")
        .context_size_alerts
        .clear();
    h.queue_outer_turn_finished_context_size_alerts(&cid, &outer_turn_id);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .filter(|prompt| prompt.is_context_size_alert())
            .count(),
        1
    );
    arm(h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent"));
    h.queue_outer_turn_finished_context_size_alerts(&cid, &outer_turn_id);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .filter(|prompt| prompt.is_context_size_alert())
            .count(),
        1,
        "high usage must not refire the same alert"
    );
    h.shutdown().expect("shutdown");
}

/// Runtime deadline processing completes a registered input wait normally,
/// clears its foreground tracking, and does not suspend the outer turn.
#[test]
fn input_wait_timeout_completes_once_inside_running_turn() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call = wait_input_call("wait-input-timeout");
    seed_tools_running(
        &mut h,
        &cid,
        vec![call.id.clone(), ToolCallId::from("still-running-sibling")],
    );
    h.handle_wait_tool_call(&cid, &call, ToolName::new("wait"))
        .expect("register input wait");
    let deadline = h.next_input_wait_deadline().expect("input deadline");
    assert_eq!(h.next_runtime_deadline(), Some(deadline));

    h.process_runtime_deadlines_at(deadline);
    assert_eq!(tool_result_count(&h, call.id.as_str()), 1);
    assert!(!h.input_wait_pending_for(&cid));
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&call.id)
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id == call.id
                && result.result == CborValue::Map(vec![(
                    CborValue::Text("timed_out".to_owned()),
                    CborValue::Bool(true),
                )])
                && result.display.as_ref().is_some_and(|display|
                    display.status == tau_proto::ToolUseStatus::Warning
                        && display.status_text == "timeout")
    )));
    h.process_runtime_deadlines_at(deadline);
    h.activate_waits_for(&cid, tau_proto::ObservationId::random());
    assert_eq!(tool_result_count(&h, call.id.as_str()), 1);
    h.shutdown().expect("shutdown");
}

/// Production timeout publication adds one advisory on the third consecutive
/// activating-input timeout and leaves later timeouts in that run unadorned.
#[test]
fn repeated_input_wait_timeouts_add_one_advisory() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let has_advice = |h: &Harness, call_id: &str| {
        event_log_contains_any_source(h, |event| {
            matches!(
                event,
                Event::ToolResult(result)
                    if result.call_id.as_str() == call_id
                        && matches!(
                            &result.result,
                            CborValue::Map(entries)
                                if entries.iter().any(|(key, _)|
                                    key == &CborValue::Text("advice".to_owned()))
                        )
            )
        })
    };

    for index in 1..=4 {
        let call_id = format!("repeated-input-wait-{index}");
        let mut call = wait_input_call(&call_id);
        call.call_ref = Some(tau_proto::ToolCallRef {
            declaration: tau_proto::ObservationId::from_bytes([index; 16]),
            item_index: 0,
        });
        seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
        let now = path_std_time::Instant::now();
        h.handle_wait_tool_call_at(&cid, &call, ToolName::new("wait"), now)
            .expect("register input wait");
        h.process_input_wait_deadlines(
            h.next_input_wait_deadline()
                .expect("registered input deadline"),
        );
        assert_eq!(has_advice(&h, &call_id), index == 3);
    }
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "resumed work".to_owned(),
        )
        .expect("valid working status"),
    )
    .expect("reset guard with status report");
    for index in 5..=7 {
        let call_id = format!("repeated-input-wait-{index}");
        let mut call = wait_input_call(&call_id);
        call.call_ref = Some(tau_proto::ToolCallRef {
            declaration: tau_proto::ObservationId::from_bytes([index; 16]),
            item_index: 0,
        });
        seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
        h.handle_wait_tool_call_at(
            &cid,
            &call,
            ToolName::new("wait"),
            path_std_time::Instant::now(),
        )
        .expect("register reset input wait");
        h.process_input_wait_deadlines(
            h.next_input_wait_deadline()
                .expect("registered input deadline"),
        );
        assert_eq!(has_advice(&h, &call_id), index == 7);
    }
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Waiting,
            "await automation".to_owned(),
        )
        .expect("valid waiting status"),
    )
    .expect("report waiting");
    for index in 8..=10 {
        let call_id = format!("repeated-input-wait-{index}");
        let mut call = wait_input_call(&call_id);
        call.call_ref = Some(tau_proto::ToolCallRef {
            declaration: tau_proto::ObservationId::from_bytes([index; 16]),
            item_index: 0,
        });
        seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
        h.handle_wait_tool_call_at(
            &cid,
            &call,
            ToolName::new("wait"),
            path_std_time::Instant::now(),
        )
        .expect("register waiting-status input wait");
        h.process_input_wait_deadlines(
            h.next_input_wait_deadline()
                .expect("registered input deadline"),
        );
        assert!(!has_advice(&h, &call_id));
    }
    h.shutdown().expect("shutdown");
}

/// A stale generic publish-idle obligation must not checkpoint a continuation
/// while the owning provider turn is still blocked in a foreground input wait.
///
/// The historical failure committed such a checkpoint, then refused to send its
/// prompt because the foreground round was open. That left an unrecoverable
/// `DispatchUncertain` owner with no corresponding provider request.
#[test]
fn deferred_dispatch_waits_for_open_foreground_round_to_finish() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("enter input wait".to_owned()))
        .expect("dispatch initial inference");
    let initial_prompt = read_nth_prompt_created(&h, 0);
    let initial_provider =
        h.provider_runtime.pending_prompts[&initial_prompt.agent_prompt_id].clone();
    h.handle_extension_event_inner(
        &initial_provider,
        Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: initial_prompt.agent_prompt_id.clone(),
            originator: initial_prompt.originator.clone(),
        }),
    )
    .expect("record initial provider submission");

    let wait_call_id = ToolCallId::from("wait-deferred-open-round");
    h.handle_provider_response_finished(provider_input_wait_response(
        &initial_prompt,
        wait_call_id.as_str(),
        10,
    ))
    .expect("provider opens foreground input wait");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolStarted(started) if started.call_id == wait_call_id
    )));
    assert!(h.input_wait_pending_for(&cid));
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));
    assert!(h.agent_has_open_foreground_tool_round(&cid));
    let open_round_activation_cut = h
        .activation_cut_before_current_head(&cid)
        .expect("closed prefix before open-round response");

    let events_before_drain = event_log_events(&h);
    let checkpoints_before = events_before_drain
        .iter()
        .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
        .count();
    let starts_before = events_before_drain
        .iter()
        .filter(|event| matches!(event, Event::AgentPromptStarted(_)))
        .count();
    let prompts_before = events_before_drain
        .iter()
        .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
        .count();
    let submissions_before = events_before_drain
        .iter()
        .filter(|event| matches!(event, Event::ProviderPromptSubmitted(_)))
        .count();
    h.runtime_io.publication.idle_dispatches.push_back(
        path_crate_harness::interception::DeferredPromptDispatch {
            activation_source_seq: None,
            cid: cid.clone(),
            activation_cut: None,
            activation_through: None,
            obligation: DeferredActivationObligation::OrdinaryPublishIdle,
        },
    );

    h.drain_publish_idle_dispatches();

    let events_while_waiting = event_log_events(&h);
    assert_eq!(
        events_while_waiting
            .iter()
            .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        checkpoints_before,
        "an open foreground round must block checkpoint creation"
    );
    assert_eq!(
        events_while_waiting
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptStarted(_)))
            .count(),
        starts_before
    );
    assert_eq!(
        events_while_waiting
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        prompts_before
    );
    assert_eq!(
        events_while_waiting
            .iter()
            .filter(|event| matches!(event, Event::ProviderPromptSubmitted(_)))
            .count(),
        submissions_before
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert_eq!(
        h.runtime_io
            .publication
            .idle_dispatches
            .iter()
            .filter(|deferred| deferred.cid == cid && !deferred.obligation.is_committed())
            .count(),
        1,
        "the stale generic obligation remains queued exactly once"
    );

    let deadline = h.next_input_wait_deadline().expect("input-wait deadline");
    h.process_runtime_deadlines_at(deadline);
    assert!(!h.input_wait_pending_for(&cid));
    assert!(!h.agent_has_open_foreground_tool_round(&cid));
    let through = tau_proto::AgentHead::Node(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("wait terminal"),
    );
    let continuation = read_nth_prompt_created(&h, 1);
    let continuation_provider =
        h.provider_runtime.pending_prompts[&continuation.agent_prompt_id].clone();
    h.handle_extension_event_inner(
        &continuation_provider,
        Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: continuation.agent_prompt_id.clone(),
            originator: continuation.originator.clone(),
        }),
    )
    .expect("record continuation provider submission");

    let events = event_log_events(&h);
    let checkpoints = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == continuation.agent_prompt_id =>
            {
                Some(started)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].through, through);
    assert_eq!(checkpoints[0].model, Some("test/model".into()));
    assert_eq!(
        checkpoints[0].operation,
        Some(tau_proto::PromptOperation::Inference)
    );
    assert_eq!(
        checkpoints[0].activation_cut,
        Some(open_round_activation_cut)
    );
    let sequence = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == continuation.agent_prompt_id =>
            {
                Some("checkpoint")
            }
            Event::AgentPromptStarted(started)
                if started.agent_prompt_id == continuation.agent_prompt_id =>
            {
                Some("started")
            }
            Event::AgentPromptCreated(created)
                if created.agent_prompt_id == continuation.agent_prompt_id =>
            {
                Some("created")
            }
            Event::ProviderPromptSubmitted(submitted)
                if submitted.agent_prompt_id == continuation.agent_prompt_id =>
            {
                Some("submitted")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(sequence, ["checkpoint", "started", "created", "submitted"]);
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    h.shutdown().expect("shutdown");
}

/// Visible input that settles an activating wait must release the complete
/// foreground tool round and dispatch one continuation. A second visible input
/// remains ordered behind that continuation instead of leaving the agent
/// permanently idle with queued activation.
#[test]
fn activating_wait_settlement_dispatches_once_and_preserves_next_input() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("enter input wait".to_owned()))
        .expect("dispatch initial inference");
    let initial_prompt = read_nth_prompt_created(&h, 0);
    let initial_provider =
        h.provider_runtime.pending_prompts[&initial_prompt.agent_prompt_id].clone();
    h.handle_extension_event_inner(
        &initial_provider,
        Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: initial_prompt.agent_prompt_id.clone(),
            originator: initial_prompt.originator.clone(),
        }),
    )
    .expect("record initial provider submission");
    h.handle_provider_response_finished(provider_input_wait_response(
        &initial_prompt,
        "activating-wait-settlement",
        60,
    ))
    .expect("open activating wait");
    assert!(h.input_wait_pending_for(&cid));

    let checkpoints_before = event_log_count(&h, |event| {
        matches!(event, Event::AgentInferenceDispatchStarted(_))
    });
    assert_eq!(
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            agent_id.as_str(),
            PendingPrompt::user("first visible activation".to_owned()),
        )
        .expect("submit first activation"),
        PromptSubmission::Queued
    );
    assert!(!h.input_wait_pending_for(&cid));
    assert_eq!(tool_result_count(&h, "activating-wait-settlement"), 1);
    assert_eq!(
        event_log_count(&h, |event| {
            matches!(event, Event::AgentInferenceDispatchStarted(_))
        }),
        checkpoints_before + 1,
        "settlement releases exactly one post-tool continuation"
    );
    let continuation_id = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("continuation prompt");
    let continuation = read_prompt_created(&h, &continuation_id);
    assert!(continuation.context.flatten().iter().any(|item| {
        text_part(item).is_some_and(|text| text.contains("first visible activation"))
    }));
    assert_eq!(
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            agent_id.as_str(),
            PendingPrompt::user("second visible activation".to_owned()),
        )
        .expect("submit second activation"),
        PromptSubmission::Queued
    );
    h.handle_provider_response_finished(provider_text_response(
        &continuation.agent_prompt_id,
        agent_id.clone(),
        "continuation complete",
    ))
    .expect("finish continuation");
    let next_prompt_id = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("second activation prompt");
    let next_prompt = read_prompt_created(&h, &next_prompt_id);
    assert!(next_prompt.context.flatten().iter().any(|item| {
        text_part(item).is_some_and(|text| text.contains("second visible activation"))
    }));
    assert!(h.runtime_io.publication.deferred.is_empty());
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// A peer-created extension side conversation keeps its harness extension
/// originator while visible user input settles an activating wait. The settled
/// terminal and steered activation must still release exactly one continuation.
#[test]
fn peer_entrypoint_activating_wait_settlement_dispatches_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    configure_inter_session_receivers(&mut h, &[("engineer", true)]);
    let received = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "peer-activating-wait".to_owned(),
            message_id: tau_proto::AgentMessageId::parse("peer-activating-wait-message")
                .expect("message id"),
            capability: "test-only".to_owned(),
            sender_session_id: test_session_id("sender-session"),
            sender_id: crate::parse_agent_id("sender-agent"),
            recipient_session_id: h.session_runtime.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "wait for visible input".to_owned(),
        },
    );
    assert_eq!(received.failure, None);
    assert!(received.started);
    let agent_id = received.recipient_id.expect("peer endpoint");
    let cid = h.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .peer_entrypoint_endpoint
    );
    let initial_prompt = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(prompt_id, prompt_cid)| {
            (prompt_cid == &cid).then(|| read_prompt_created(&h, prompt_id))
        })
        .expect("side prompt");
    assert!(matches!(
        initial_prompt.originator,
        tau_proto::PromptOriginator::Extension { .. }
    ));
    let mut wait_response =
        provider_input_wait_response(&initial_prompt, "side-activating-wait", 60);
    wait_response.originator = initial_prompt.originator.clone();
    wait_response.output_items.insert(
        0,
        ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text: "wait for visible input".to_owned(),
        }),
    );
    h.handle_provider_response_finished(wait_response)
        .expect("open activating wait");
    assert!(h.input_wait_pending_for(&cid));

    let checkpoints_before = event_log_count(&h, |event| {
        matches!(event, Event::AgentInferenceDispatchStarted(_))
    });
    let interactions_before = event_log_count(&h, |event| {
        matches!(
            event,
            Event::AgentUserInteractionRecorded(interaction) if interaction.agent_id == agent_id
        )
    });
    h.record_accepted_visible_user_interaction(agent_id.as_str())
        .expect("record first visible interaction");
    assert_eq!(
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            agent_id.as_str(),
            PendingPrompt::human_ui_watch_notified("first visible activation".to_owned()),
        )
        .expect("submit first activation"),
        PromptSubmission::Queued
    );
    assert_eq!(tool_result_count(&h, "side-activating-wait"), 1);
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::AgentUserInteractionRecorded(interaction) if interaction.agent_id == agent_id
        )),
        interactions_before + 1
    );
    assert_eq!(
        event_log_count(&h, |event| {
            matches!(event, Event::AgentInferenceDispatchStarted(_))
        }),
        checkpoints_before + 1,
        "settlement releases exactly one side-conversation continuation"
    );
    let continuation_id = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("continuation prompt");
    let continuation = read_prompt_created(&h, &continuation_id);
    assert_eq!(continuation.originator, initial_prompt.originator);
    assert!(continuation.context.flatten().iter().any(|item| {
        text_part(item).is_some_and(|text| text.contains("first visible activation"))
    }));
    let settlement_order = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("peer journal")
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentToolTerminalClassified(_) => Some("classified"),
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == "side-activating-wait" =>
            {
                Some("terminal")
            }
            Event::AgentToolWaitSettled(_) => Some("settled"),
            Event::AgentPromptSteered(steered)
                if steered.agent_id == agent_id
                    && steered.text == "first visible activation"
                    && steered.submission_source == tau_proto::PromptSubmissionSource::HumanUi =>
            {
                Some("steered")
            }
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == continuation_id =>
            {
                Some("checkpoint")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        settlement_order,
        ["classified", "terminal", "settled", "steered", "checkpoint"]
    );

    h.record_accepted_visible_user_interaction(agent_id.as_str())
        .expect("record second visible interaction");
    assert_eq!(
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            agent_id.as_str(),
            PendingPrompt::human_ui_watch_notified("second visible activation".to_owned()),
        )
        .expect("submit second activation"),
        PromptSubmission::Queued
    );
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::AgentUserInteractionRecorded(interaction) if interaction.agent_id == agent_id
        )),
        interactions_before + 2
    );
    let mut continuation_response = provider_text_response(
        &continuation.agent_prompt_id,
        agent_id.clone(),
        "continuation complete",
    );
    continuation_response.originator = continuation.originator.clone();
    h.handle_provider_response_finished(continuation_response)
        .expect("finish continuation");
    let next_prompt_id = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("second activation prompt");
    let next_prompt = read_prompt_created(&h, &next_prompt_id);
    assert!(next_prompt.context.flatten().iter().any(|item| {
        text_part(item).is_some_and(|text| text.contains("second visible activation"))
    }));
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    h.shutdown().expect("shutdown");
}

/// A crash after the wait terminal and visible-input steer commit, but before
/// the inference checkpoint commits, leaves the durable steer as the recovery
/// owner. Resume dispatches it once without repairing the already-complete
/// wait.
#[test]
fn peer_entrypoint_activating_wait_restart_recovers_committed_steer_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let agent_id = {
        let mut h = quiet_provider_harness(&state).expect("start");
        configure_inter_session_receivers(&mut h, &[("engineer", true)]);
        let received = h.handle_external_agent_message_request_without_auth_for_test(
            tau_proto::ExternalAgentMessageRequest {
                request_id: "peer-activating-wait-restart".to_owned(),
                message_id: tau_proto::AgentMessageId::parse(
                    "peer-activating-wait-restart-message",
                )
                .expect("message id"),
                capability: "test-only".to_owned(),
                sender_session_id: test_session_id("sender-session"),
                sender_id: crate::parse_agent_id("sender-agent"),
                recipient_session_id: h.session_runtime.current_session_id.clone(),
                recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
                kind: tau_proto::AgentMessageKind::Message,
                message: "wait for visible input".to_owned(),
            },
        );
        assert_eq!(received.failure, None);
        assert!(received.started);
        let agent_id = received.recipient_id.expect("peer endpoint");
        let cid = h.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
        let initial_prompt = h
            .prompt_coordination
            .prompt_runtime
            .agents
            .iter()
            .find_map(|(prompt_id, prompt_cid)| {
                (prompt_cid == &cid).then(|| read_prompt_created(&h, prompt_id))
            })
            .expect("side prompt");
        let mut wait_response =
            provider_input_wait_response(&initial_prompt, "restart-side-activating-wait", 60);
        wait_response.originator = initial_prompt.originator.clone();
        wait_response.output_items.insert(
            0,
            ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "wait for visible input".to_owned(),
            }),
        );
        h.handle_provider_response_finished(wait_response)
            .expect("open activating wait");

        let _checkpoint_interceptor = connect_test_tool(&mut h, "wait-checkpoint-interceptor");
        h.handle_extension_event(
            "wait-checkpoint-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register checkpoint interceptor");
        let checkpoints_before = event_log_count(&h, |event| {
            matches!(
                event,
                Event::AgentInferenceDispatchStarted(started) if started.agent_id == agent_id
            )
        });
        h.record_accepted_visible_user_interaction(agent_id.as_str())
            .expect("record first visible interaction");
        assert_eq!(
            h.submit_prompt_to_agent(
                h.session_runtime.current_session_id.clone(),
                agent_id.as_str(),
                PendingPrompt::human_ui_watch_notified("first visible activation".to_owned()),
            )
            .expect("submit first activation"),
            PromptSubmission::Queued
        );
        assert_eq!(tool_result_count(&h, "restart-side-activating-wait"), 1);
        assert!(event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::AgentPromptSteered(steered)
                if steered.agent_id == agent_id
                    && steered.text == "first visible activation"
                    && steered.submission_source
                        == tau_proto::PromptSubmissionSource::HumanUi
        )));
        assert_eq!(
            event_log_count(&h, |event| matches!(
                event,
                Event::AgentInferenceDispatchStarted(started) if started.agent_id == agent_id
            )),
            checkpoints_before
        );
        assert!(h.runtime_io.publication.pending_intercept.is_some());
        assert!(
            h.session_runtime.persistence_owner.as_ref().is_some_and(
                |owner| owner.wait_for_latest_durability_for_test(Duration::from_secs(5))
            )
        );
        drop(h);
        agent_id
    };

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume after checkpoint cut");
        let cid = h.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
        assert_eq!(
            event_log_count(&h, |event| matches!(
                event,
                Event::AgentInferenceDispatchStarted(started) if started.agent_id == agent_id
            )),
            1,
            "resume dispatches the uncovered durable steer once"
        );
        assert!(!event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::ProviderToolError(error)
                if error.call_id.as_str() == "restart-side-activating-wait"
        )));
        let continuation_id = h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .in_flight_prompt
            .clone()
            .expect("recovered continuation");
        let continuation = read_prompt_created(&h, &continuation_id);
        assert!(matches!(
            continuation.originator,
            tau_proto::PromptOriginator::Extension { .. }
        ));
        assert!(continuation.context.flatten().iter().any(|item| {
            text_part(item).is_some_and(|text| text.contains("first visible activation"))
        }));
        let mut continuation_response =
            provider_text_response(&continuation_id, agent_id.clone(), "continuation complete");
        continuation_response.originator = continuation.originator.clone();
        h.handle_provider_response_finished(continuation_response)
            .expect("finish recovered continuation");
        h.shutdown().expect("shutdown recovered harness");
    }

    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("second resume");
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentInferenceDispatchStarted(started)
            if started.agent_id == agent_id
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolError(error)
            if error.call_id.as_str() == "restart-side-activating-wait"
    )));
    h.shutdown().expect("shutdown second resume");
}

/// Committed endpoint unload crosses the production lifecycle boundary and
/// drops runtime-only input waits, retained completion/checkpoint owners,
/// attempt markers, and deferred activation obligations before removal.
#[test]
fn agent_unload_discards_registered_input_wait() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let durable_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    let call = wait_input_call("wait-input-unload");
    seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
    h.handle_wait_tool_call(&cid, &call, ToolName::new("wait"))
        .expect("register input wait");
    assert!(h.input_wait_pending_for(&cid));
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-unload-retry").expect("transaction");
    h.prompt_coordination
        .prompt_runtime
        .pending_publish_completions
        .insert(
            cid.clone(),
            AgentPublishCompletion::StandaloneContinuation {
                transaction_id: transaction_id.clone(),
                model: "test/model".into(),
                activation_cut: tau_proto::AgentHead::Root,
                batch_parent: tau_proto::AgentHead::Root,
                source: None,
                retry_prompts: vec![PendingPrompt::user("stale unload retry".to_owned())],
                complete_on_commit: true,
                approved_retry_event: None,
            },
        );
    h.prompt_coordination
        .compaction_runtime
        .enqueued_inference_checkpoints
        .insert((crate::parse_agent_id(&durable_id), transaction_id));
    h.enqueue_committed_activation_dispatch(
        cid.clone(),
        Some(tau_proto::AgentHead::Root),
        Some(tau_proto::AgentHead::Root),
    );

    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: crate::parse_agent_id(&durable_id),
        }),
    );
    assert!(!h.input_wait_pending_for(&cid));
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .contains_key(&cid)
    );
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .enqueued_inference_checkpoints
            .is_empty()
    );
    assert!(
        h.runtime_io
            .publication
            .idle_dispatches
            .iter()
            .all(|dispatch| dispatch.cid != cid)
    );
    h.activate_waits_for(&cid, tau_proto::ObservationId::random());
    assert_eq!(tool_result_count(&h, call.id.as_str()), 0);
    h.shutdown().expect("shutdown");
}

/// Exact `wait` is scoped to the background call owner before any waiter is
/// registered. A cross-owner wait should fail immediately rather than creating
/// active wait state that later messages could interrupt.
#[test]
fn cross_owner_exact_wait_is_rejected_without_active_wait_state() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-cross-msg-wait");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-cross-msg-wait"),
        instant_background_test_tool_spec("slow_cross_msg_wait"),
    );

    let target_cid = ensure_test_user_agent(&mut h);
    let waiter_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let target_agent_id = h.agent_runtime.agent_registry.agents[&target_cid]
        .identity
        .agent_id
        .clone()
        .expect("target agent id");
    let waiter_agent_id = h.agent_runtime.agent_registry.agents[&waiter_cid]
        .identity
        .agent_id
        .clone()
        .expect("waiter agent id");
    finish_test_agent_context_wait(
        &mut h,
        &tau_proto::AgentId::parse(&waiter_agent_id).expect("agent id"),
    );

    let background_call_id: ToolCallId = "bg-cross-msg-wait".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &target_cid,
        background_call_id.as_str(),
        "slow_cross_msg_wait",
    );

    let wait_call_id: ToolCallId = "wait-cross-msg-interrupt".into();
    let wait_call = AgentToolCall {
        call_ref: None,
        id: wait_call_id.clone(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(background_call_id.to_string()),
        )]),
    };
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&waiter_cid)
        .expect("waiter agent")
        .dispatch
        .pending_prompts
        .push_back(PendingPrompt::user("queued waiter input".to_owned()));
    h.handle_wait_tool_call(&waiter_cid, &wait_call, ToolName::new("wait"))
        .expect("reject cross-owner wait before queued-input preemption");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == wait_call_id.as_str()
                && error.message == "unknown tool call: `bg-cross-msg-wait`"
    )));

    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("test-message-to-target-owner")
                .expect("test identifier must satisfy its grammar"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&target_agent_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "target owner only".to_owned(),
        }),
    );

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("interrupted because new input is queued"))
    )));

    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("test-message-to-wait-owner")
                .expect("test identifier must satisfy its grammar"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&waiter_agent_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "waiter should resume".to_owned(),
        }),
    );

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("interrupted because new input is queued"))
    )));
    h.shutdown().expect("shutdown");
}

/// A crash after an activating prompt occurrence but before its marked owner's
/// response closes that owner as Stale, materializes the exact occurrence, and
/// dispatches one successor. Prompt, typed-message, and raw-fact ingress share
/// this rule.
#[test]
fn resume_supersedes_uncertain_v1_owner_for_each_activation_variant() {
    let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
    let cases = [
        (
            "injected deferred Q",
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                inference_activation: true,
                agent_id: agent_id.clone(),
                text: "injected deferred Q".to_owned(),
                message_class: tau_proto::PromptMessageClass::Internal,
            }),
        ),
        (
            "steered deferred Q",
            Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                self_compaction_terminal: None,
                inference_activation: true,
                submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
                agent_id: agent_id.clone(),
                text: "steered deferred Q".to_owned(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                ctx_id: Some("deferred-q".to_owned()),
            }),
        ),
        (
            "typed message deferred Q",
            Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
                message_id: tau_proto::AgentMessageId::parse("typed-deferred-q")
                    .expect("message id"),
                sender_id: tau_proto::AgentId::parse("sender").expect("sender"),
                sender_session_id: None,
                recipient_id: agent_id.clone(),
                kind: tau_proto::AgentMessageKind::Message,
                watch_provider_status: None,
                watch_work_status: None,
                watch_long_wait: None,
                watch_lifecycle: None,
                message: "typed message deferred Q".to_owned(),
            }),
        ),
        (
            "raw fact deferred Q",
            Event::MessageDelivered(tau_proto::MessageDelivered::new(
                tau_proto::MessagePublisherId::parse("external").expect("publisher"),
                tau_proto::MessageAgentTarget::new(agent_id.as_str()),
                tau_proto::MessageFactId::new("raw-deferred-q"),
                tau_proto::MessageParty {
                    stable_id: "external".to_owned(),
                    display_name: None,
                    sender_auth: None,
                },
                None,
                "raw fact deferred Q",
            )),
        ),
    ];

    for (text, activation) in cases {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        seed_main_agent_loaded(&state);
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
        let owner = test_agent_prompt_id(format!("ap-{}", text.replace(' ', "-")));
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
        if activation.message_agent_target().is_some() {
            store
                .append_agent_message_fact_at(
                    "main",
                    None,
                    activation,
                    tau_proto::UnixMicros::now(),
                )
                .expect("append raw activating fact");
        } else {
            append_seed_agent_event(&mut store, activation);
        }
        assert!(
            store
                .agent("main")
                .and_then(|tree| tree
                    .node_for_durable_event_seq(tau_core::PersistedAgentEventSeq::new(3)))
                .is_none(),
            "the deferred occurrence has no node before closure"
        );
        drop(store);

        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let prompt = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::AgentPromptCreated(prompt) => Some(prompt),
                _ => None,
            })
            .unwrap_or_else(|| {
                let cid = test_user_agent(&h);
                panic!(
                    "{text}: no successor; state={:?}, dispatch={:?}, wakes={:?}, events={:?}",
                    h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
                    h.agent_runtime.agent_registry.agents[&cid]
                        .dispatch
                        .activation_dispatch,
                    h.agent_runtime.agent_registry.agents[&cid]
                        .dispatch
                        .pending_message_wakes,
                    event_log_events(&h)
                        .iter()
                        .map(Event::name)
                        .collect::<Vec<_>>()
                )
            });
        let rendered = serde_json::to_string(&prompt.context).expect("context");
        assert_eq!(rendered.matches(text).count(), 1);
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::AgentPromptTerminated(terminated)
                        if terminated.agent_prompt_id == owner
                            && terminated.reason
                                == tau_proto::AgentPromptTerminationReason::Stale
                ))
                .count(),
            1,
            "restore closes the uncertain owner exactly once"
        );
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
                .count(),
            1,
            "one deferred occurrence creates one successor"
        );
        h.shutdown().expect("shutdown");
    }
}

/// Explicit-false canonical facts are passive transcript context and must not
/// independently wake inference during cold replay.
#[test]
fn resume_does_not_dispatch_false_canonical_facts() {
    let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
    let cases = [
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id.clone(),
            text: "passive submitted".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            inference_activation: false,
            agent_id: agent_id.clone(),
            text: "passive injected".to_owned(),
            message_class: tau_proto::PromptMessageClass::Internal,
        }),
        Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
            self_compaction_terminal: None,
            inference_activation: false,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            agent_id,
            text: "passive steered".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: None,
            ctx_id: None,
        }),
    ];

    for event in cases {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        seed_inference_activation_event(&state, event);

        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        assert!(!event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::AgentPromptCreated(_) | Event::AgentInferenceDispatchStarted(_)
        )));
        h.shutdown().expect("shutdown");
    }
}

/// Every provider-qualified wait declaration records a typed pre-resolution
/// observation, including malformed arguments and unresolved exact targets.
#[test]
fn wait_observation_classifies_invalid_and_unresolved_exact_arguments() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = harness.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent id");

    for (byte, call_id, arguments, expected_mode) in [
        (
            31,
            "invalid-wait",
            CborValue::Text("not a map".into()),
            tau_proto::ToolWaitMode::InvalidArguments,
        ),
        (
            32,
            "unresolved-wait",
            CborValue::Map(vec![(
                CborValue::Text("tool_call_id".into()),
                CborValue::Text("missing-target".into()),
            )]),
            tau_proto::ToolWaitMode::ExactUnresolved,
        ),
    ] {
        let call_ref = tau_proto::ToolCallRef {
            declaration: tau_proto::ObservationId::from_bytes([byte; 16]),
            item_index: 0,
        };
        harness
            .handle_wait_tool_call(
                &cid,
                &AgentToolCall {
                    call_ref: Some(call_ref),
                    id: call_id.into(),
                    name: ToolName::new("wait"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments,
                },
                ToolName::new("wait"),
            )
            .expect("handle wait");
        assert!(
            harness
                .session_runtime
                .agent_store
                .agent_events(&agent_id)
                .expect("agent records")
                .iter()
                .any(|record| matches!(
                    &record.event,
                    Event::AgentToolWaitObserved(observed)
                        if observed.wait_call == call_ref && observed.mode == expected_mode
                ))
        );
    }
}
