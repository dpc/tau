//! Tests for session lifecycle behavior.

use super::*;

/// Runtime construction carries configured input-wait bounds through the
/// internal-tool facade, session rollover, and the actual wait registration.
#[test]
fn configured_input_wait_bounds_survive_rollover_and_drive_wait_registration() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let config_dir = state_dir.join("config");
    path_std_fs::create_dir_all(&config_dir).expect("config directory");
    path_std_fs::write(
        config_dir.join("harness.yaml"),
        "wait_timeout_minimum_minutes: 7\nwait_timeout_maximum_minutes: 9\n",
    )
    .expect("configured wait bounds");

    let mut h = echo_harness(&state_dir).expect("start");
    let input_wait_arguments = CborValue::Map(vec![(
        CborValue::Text("timeout_minutes".to_owned()),
        CborValue::Integer(1.into()),
    )]);
    assert_eq!(h.input_wait_timeout_bounds().minimum().get(), 7);
    assert_eq!(h.input_wait_timeout_bounds().maximum().get(), 9);
    assert_eq!(
        path_crate_internal_tools::InternalToolHost::new(&mut h)
            .normalized_wait_timeout_minutes(&input_wait_arguments),
        Ok(Some(7))
    );

    h.switch_session(
        test_session_id("configured-wait-bounds"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");
    assert_eq!(h.input_wait_timeout_bounds().minimum().get(), 7);
    assert_eq!(h.input_wait_timeout_bounds().maximum().get(), 9);

    let cid = ensure_test_user_agent(&mut h);
    let call = AgentToolCall {
        call_ref: None,
        id: "configured-wait-bounds".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: input_wait_arguments,
    };
    seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
    let now = path_std_time::Instant::now();
    h.handle_wait_tool_call_at(&cid, &call, ToolName::new("wait"), now)
        .expect("register configured input wait");
    assert_eq!(
        h.next_input_wait_deadline(),
        now.checked_add(path_std_time::Duration::from_secs(7 * 60))
    );
    h.shutdown().expect("shutdown");
}

/// Each new canonical activation committed before its first checkpoint is
/// replay-woken once.
#[test]
fn resume_dispatches_true_activation_without_first_checkpoint() {
    let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
    let cases = [
        (
            "submitted activation",
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: true,
                agent_id: agent_id.clone(),
                text: "submitted activation".to_owned(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        ),
        (
            "injected activation",
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                inference_activation: true,
                agent_id: agent_id.clone(),
                text: "injected activation".to_owned(),
                message_class: tau_proto::PromptMessageClass::Internal,
            }),
        ),
        (
            "steered activation",
            Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                self_compaction_terminal: None,
                inference_activation: true,
                submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
                agent_id,
                text: "steered activation".to_owned(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                ctx_id: Some("ctx-1".to_owned()),
            }),
        ),
    ];

    for (text, event) in cases {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        seed_inference_activation_event(&state, event);

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
        let context = serde_json::to_string(&prompt.context).expect("context");
        assert_eq!(context.matches(text).count(), 1);
        assert_eq!(
            event_log_events(&h)
                .into_iter()
                .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
                .count(),
            1
        );
        h.shutdown().expect("shutdown");
    }
}
/// Comparable readiness-deferred activations remain distinct until one
/// selected-branch checkpoint durably covers and transfers both obligations.
#[test]
fn readiness_deferred_linear_activations_share_one_checkpoint() {
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

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("linear activation A".to_owned()))
        .expect("park A");
    let branch_a = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("A");
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("linear activation B".to_owned()))
        .expect("park B");
    let branch_b = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("B");
    assert_ne!(branch_a, branch_b);
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 2);

    finish_test_agent_context_wait(&mut h, &agent_id);
    h.drain_publish_idle_dispatches();
    let checkpoints = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].through, tau_proto::AgentHead::Node(branch_b));
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
}

/// The restore notice is a one-shot durable fact. Follow-up prompts and later
/// cold resumes may replay the original notice in history, but must not append
/// another copy.
#[test]
fn restore_notice_is_not_duplicated_by_followups_or_later_resumes() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_prior_user_message(&sp, "before restore");

    let notice = {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("first resume");

        h.submit_user_prompt(test_session_id("s1"), "first after restore".to_owned())
            .expect("submit first resumed prompt");
        let first_prompt = read_nth_prompt_created(&h, 0);
        let first_spid = first_prompt.agent_prompt_id.clone();
        let notice = restore_notice_context_text(&first_prompt)
            .expect("restore notice")
            .to_owned();
        assert_eq!(restore_notice_context_count(&first_prompt), 1);

        h.handle_provider_response_finished(provider_text_response(
            &first_spid,
            first_prompt.agent_id.clone(),
            "first answer",
        ))
        .expect("finish first prompt");
        h.submit_user_prompt(test_session_id("s1"), "second after restore".to_owned())
            .expect("submit second prompt");
        let second_prompt = read_nth_prompt_created(&h, 1);
        assert_eq!(context_text_count(&second_prompt, notice.as_str()), 1);
        assert_eq!(restore_notice_context_count(&second_prompt), 1);
        assert_eq!(restore_notice_event_count(&h), 1);
        h.handle_provider_response_finished(provider_text_response(
            &second_prompt.agent_prompt_id,
            second_prompt.agent_id.clone(),
            "second answer",
        ))
        .expect("finish second prompt before cold resume");

        h.shutdown().expect("shutdown");
        notice
    };
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("second resume");

        h.submit_user_prompt(test_session_id("s1"), "third after restore".to_owned())
            .expect("submit after second resume");
        let prompt = read_nth_prompt_created(&h, 0);
        assert_eq!(context_text_count(&prompt, notice.as_str()), 1);
        assert_eq!(restore_notice_context_count(&prompt), 1);
        assert_eq!(restore_notice_event_count(&h), 1);

        h.shutdown().expect("shutdown");
    }
}

/// Restored no-arg waits must replay completions by durable completion order,
/// not by the earlier provider-placeholder order.
#[test]
fn resumed_no_arg_wait_uses_restored_completion_event_order() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "restored-a", "slow_bg");
    seed_background_placeholder(&sp, "restored-b", "slow_bg");
    for (call_id, text) in [
        ("restored-b", "first restored output"),
        ("restored-a", "second restored output"),
    ] {
        seed_background_result(&sp, call_id, "slow_bg", text);
    }

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let cid = ensure_test_user_agent(&mut h);
    h.handle_wait_tool_call(
        &cid,
        &wait_no_args_call("wait-restored-first"),
        ToolName::new("wait"),
    )
    .expect("consume first restored completion");
    h.handle_wait_tool_call(
        &cid,
        &wait_no_args_call("wait-restored-second"),
        ToolName::new("wait"),
    )
    .expect("consume second restored completion");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-restored-first"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some("restored-b")
                && cbor_map_text(&result.result, "output") == Some("first restored output")
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-restored-second"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some("restored-a")
                && cbor_map_text(&result.result, "output") == Some("second restored output")
    )));

    h.shutdown().expect("shutdown");
}

/// Ensures daemon-style injected handlers observe restored activation dispatch
/// because they are installed before rehydration and provider readiness.
#[test]
fn resume_installs_internal_handlers_before_restored_activation_dispatch() {
    struct PromptObserver(std::sync::Arc<path_std_sync::atomic::AtomicBool>);

    impl crate::InternalToolHandler for PromptObserver {
        fn tool_specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }

        fn handles(&self, _internal_tool_name: &ToolName) -> bool {
            false
        }

        fn handle_event(
            &self,
            _host: &mut crate::InternalToolHost<'_>,
            event: &Event,
        ) -> Result<(), HarnessError> {
            if matches!(event, Event::AgentPromptCreated(_)) {
                self.0.store(true, path_std_sync_atomic::Ordering::SeqCst);
            }
            Ok(())
        }
    }

    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let agent_id = tau_proto::AgentId::parse("restored-main").expect("agent id");
    {
        let sessions_dir = tau_config::settings::sessions_dir_of(&state);
        let mut sessions = tau_core::SessionStore::open(&sessions_dir).expect("session store");
        sessions
            .record_session_meta("s1")
            .expect("seed canonical session manifest");
        sessions
            .append_session_event(
                "s1",
                None,
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                        .expect("test identifier must be valid"),

                    session_id: test_session_id("s1"),
                    agent_id: agent_id.clone(),
                    ephemeral: false,
                }),
            )
            .expect("seed membership");
        let mut agents = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
        agents
            .append_agent_event(
                agent_id.as_str(),
                None,
                Event::AgentStarted(tau_proto::AgentStarted {
                    creator: Some(tau_proto::AgentCreator::default()),

                    agent_id: agent_id.clone(),
                    parent_agent: None,
                    role: "engineer".to_owned(),
                    display_name: None,
                    metadata: Vec::new(),
                    ephemeral: false,
                }),
            )
            .expect("seed creation");
        agents
            .append_agent_event(
                agent_id.as_str(),
                None,
                Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                    agent_id: agent_id.clone(),
                    text: "resume outstanding activation".to_owned(),
                    trusted_internal_spans: Vec::new(),
                    message_class: tau_proto::PromptMessageClass::User,
                    internal_kind: None,
                    originator: tau_proto::PromptOriginator::User,
                    inference_activation: true,
                    submission_source: Default::default(),
                    display_name: None,
                    ctx_id: None,
                }),
            )
            .expect("seed outstanding prompt");
    }

    let observed = path_std_sync::Arc::new(path_std_sync_atomic::AtomicBool::new(false));
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state.join("config")),
        state_dir: Some(state.join("runtime")),
    };
    let mut harness = Harness::new_with_provider_and_internal_tools(
        &state,
        dirs,
        echo_runner,
        Vec::new(),
        crate::harness::TestProviderHarnessStartup {
            session_id: "s1",
            reason: tau_proto::SessionStartReason::Resume,
            storage_mode: crate::HarnessStorageMode::Durable,
            internal_tool_handlers: vec![std::sync::Arc::new(PromptObserver(observed.clone()))],
        },
    )
    .expect("resume harness");
    assert!(
        observed.load(std::sync::atomic::Ordering::SeqCst),
        "restored activation dispatched before injected handler installation"
    );
    harness.shutdown().expect("shutdown");
}

#[test]
fn resume_rehydrates_default_agent_conversation_from_durable_routing() {
    // Regression: after a cold resume the UI may know the selected agent id from
    // replay and send targeted prompts. The harness must rebuild the live
    // agent_id -> default conversation map from durable session events.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let agent_id = {
        let mut h = echo_harness(&sp).expect("start");
        h.config.selected_model = Some("test/model".into());
        h.submit_user_prompt(test_session_id("s1"), "hello".to_owned())
            .expect("submit first prompt");
        let agent_id = h
            .agent_runtime
            .agent_registry
            .agents
            .get(&test_user_agent(&h))
            .and_then(|conversation| conversation.identity.agent_id.clone())
            .expect("first prompt minted agent id");
        h.agent_runtime.agent_registry.navigation_modes.insert(
            crate::parse_agent_id(&agent_id),
            tau_proto::AgentNavigationMode::Suspended,
        );
        h.shutdown().expect("shutdown");
        agent_id
    };

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    assert_eq!(
        h.agent_runtime.agent_registry.agent_routes.get(&agent_id),
        Some(&test_user_agent(&h))
    );
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&crate::parse_agent_id(&agent_id)),
        Some(&tau_proto::AgentNavigationMode::Active)
    );
    h.shutdown().expect("shutdown");
}

/// Resume acquires writer locks and truncates torn suffixes from all semantic
/// journal classes before reconstructing runtime state. The checkpoint obstacle
/// holds a real resumed write at the journal-ahead cut so the active listing
/// cannot race through to its equally valid fresh-checkpoint state.
#[test]
fn cold_resume_recovers_agent_session_and_restore_suffixes() {
    use std::io::Write;

    let temp = TempDir::new().expect("tempdir");
    let state_dir = temp.path().join("state");
    seed_main_agent_loaded(&state_dir);
    let paths = [
        state_dir.join("agents/main/events.cbor"),
        tau_config::settings::sessions_dir_of(&state_dir).join("s1/events.cbor"),
        tau_config::settings::sessions_dir_of(&state_dir).join("s1/restore-events.cbor"),
    ];
    for path in &paths {
        std::fs::create_dir_all(path.parent().expect("journal parent")).expect("create parent");
        path_std_fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open journal")
            .write_all(&[1, 2, 3])
            .expect("append torn header");
    }
    let checkpoint_obstacle = state_dir.join("agents/main/meta.json.tmp");
    std::fs::create_dir(&checkpoint_obstacle).expect("hold checkpoint publication");

    let mut harness =
        quiet_provider_harness_with_start_reason(&state_dir, tau_proto::SessionStartReason::Resume)
            .expect("resume harness");

    assert!(harness.session_runtime.agent_store.agent("main").is_some());
    let main_id = tau_proto::AgentId::parse("main").expect("agent id");
    harness
        .session_runtime
        .persistence_owner
        .as_ref()
        .expect("durable harness has persistence owner")
        .wait_for_stream_failure_for_test(
            &tau_core::StreamIdentity::Agent(main_id.clone()),
            tau_core::PersistenceFailureKind::Sync,
        );
    let active_snapshot =
        tau_core::AgentJournalSnapshot::capture(&state_dir.join("agents"), [main_id.clone()]);
    assert!(matches!(
        active_snapshot,
        Err(tau_core::AgentStoreError::Open { source, .. })
            if source.kind() == std::io::ErrorKind::WouldBlock
    ));
    let active_entry = tau_core::list_agent_entries(&state_dir.join("agents"))
        .expect("list active agents")
        .into_iter()
        .find(|entry| entry.id == main_id)
        .expect("active main agent entry");
    assert_eq!(active_entry.status, tau_core::AgentListStatus::Busy);
    std::fs::remove_dir(&checkpoint_obstacle).expect("release checkpoint publication");
    // Resume queues fresh initialization and restore suffixes on the managed
    // persistence worker. Release those writer leases before opening offline
    // strict readers, which intentionally have no live-writer snapshot authority.
    harness.shutdown().expect("shutdown resumed harness");
    drop(harness);
    for path in &paths {
        let bytes = std::fs::read(path).expect("recovered journal");
        assert!(!bytes.ends_with(&[1, 2, 3]));
    }
    tau_core::AgentStore::open(state_dir.join("agents")).expect("strict agent replay");
    let sessions_dir = tau_config::settings::sessions_dir_of(&state_dir);
    let session_store = tau_core::SessionStore::open(&sessions_dir).expect("strict session replay");
    session_store
        .session_restore_events("s1")
        .expect("strict restore replay");
    let released_entry = tau_core::list_agent_entries(&state_dir.join("agents"))
        .expect("list agents")
        .into_iter()
        .find(|entry| entry.id == main_id)
        .expect("main agent entry");
    assert_eq!(released_entry.status, tau_core::AgentListStatus::Fresh);
}
/// A cold boot preserves one unresolved ordinary outer turn without duplicating
/// its start, then accounts a later completed turn alongside the crash
/// boundary.
#[test]
fn cold_reopen_preserves_unterminated_outer_turn_and_accounts_next_turn() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut first = echo_harness(&state).expect("first harness");
    let cid = ensure_test_user_agent(&mut first);
    let agent_id = durable_agent_id_for_conversation(&first, &cid);
    let session_id = first.session_runtime.current_session_id.clone();
    first
        .dispatch_prompt_for_agent(&cid, PendingPrompt::user("crash boundary".to_owned()))
        .expect("dispatch unresolved prompt");
    let unresolved_prompt_id = first.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("unresolved prompt");
    let expected_turn_id = tau_proto::AgentOuterTurnId::for_prompt(&unresolved_prompt_id);
    assert_eq!(
        first
            .session_runtime
            .agent_store
            .agent_events(agent_id.as_str())
            .expect("first records")
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentOuterTurnStarted(started)
                    if started.outer_turn_id == expected_turn_id
                        && started.agent_prompt_id == unresolved_prompt_id
            ))
            .count(),
        1
    );
    first.shutdown().expect("first shutdown");
    drop(first);

    let mut second = echo_harness_with_start_reason(
        session_id.as_str(),
        &state,
        tau_proto::SessionStartReason::Resume,
    )
    .expect("second harness");
    let resumed_cid = second.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
    let resumed_records = second
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("resumed records");
    assert_eq!(
        resumed_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentOuterTurnStarted(started)
                    if started.outer_turn_id == expected_turn_id
            ))
            .count(),
        1,
        "cold restore must not duplicate the unresolved start"
    );
    assert!(!resumed_records.iter().any(|record| matches!(
        &record.event,
        Event::AgentOuterTurnFinished(finished)
            if finished.outer_turn_id == expected_turn_id
    )));

    second
        .dispatch_prompt_for_agent(
            &resumed_cid,
            PendingPrompt::user("completed after restart".to_owned()),
        )
        .expect("dispatch completed prompt");
    let completed_prompt_id = second.agent_runtime.agent_registry.agents[&resumed_cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("completed prompt");
    let completed_turn_id = tau_proto::AgentOuterTurnId::for_prompt(&completed_prompt_id);
    second
        .handle_provider_response_finished(provider_text_response(
            &completed_prompt_id,
            agent_id.clone(),
            "done",
        ))
        .expect("finish post-restart prompt");
    second.shutdown().expect("second shutdown");
    drop(second);

    let stats = tau_session_inspect::read_session_stats(
        &tau_config::settings::sessions_dir_of(&state),
        &session_id,
    )
    .expect("read stats")
    .expect("session stats");
    let agent = stats
        .agents
        .iter()
        .find(|stats| stats.agent_id == agent_id)
        .expect("agent stats");
    assert_eq!(agent.totals.outer_turns_started, 2);
    assert_eq!(agent.totals.outer_turns_finished, 1);
    assert_eq!(agent.totals.outer_turns_unterminated, 1);
    let records = tau_core::AgentStore::open(state.join("agents"))
        .expect("agent store")
        .agent_events(agent_id.as_str())
        .expect("final records");
    let starts = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentOuterTurnStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0].outer_turn_id, expected_turn_id);
    assert_eq!(
        starts[1].outer_turn_id,
        tau_proto::AgentOuterTurnId::for_prompt(&completed_prompt_id)
    );
    assert_ne!(starts[0].runtime_id, starts[1].runtime_id);
    assert!(records.iter().any(|record| matches!(
        &record.event,
        Event::AgentOuterTurnFinished(finished)
            if finished.outer_turn_id == completed_turn_id
    )));
}

#[test]
fn cross_session_submission_is_rejected() {
    // The harness owns one session at a time. A UserMessage with
    // a different session id must not silently spin up a second
    // session — it gets rejected with a clear reason.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start"); // bound to "s1"

    h.config.selected_model = Some("test/model".into());
    let submission = h
        .submit_user_prompt(test_session_id("chat-1"), "hello".to_owned())
        .expect("submit");
    match submission {
        PromptSubmission::Rejected { reason } => {
            assert!(reason.contains("s1"), "reason should name bound session");
            assert!(reason.contains("chat-1"), "reason should name rejected id");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .all(|agent| agent.dispatch.pending_prompts.is_empty()),
        "rejected prompt must not queue"
    );
    assert!(
        h.session_runtime.store.session("chat-1").is_none(),
        "rejected session must not be created"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn restore_notice_elapsed_format_uses_minutes_hours_and_days() {
    // The restore notice is model-visible hidden context, so keep the elapsed
    // wording compact and deterministic while still warning about outside
    // changes since the durable transcript stopped.
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(59)))
            .contains("Less than 1 minute has passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(60)))
            .contains("1 minute has passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(42 * 60)))
            .contains("42 minutes have passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(2 * 60 * 60)))
            .contains("2 hours have passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(3 * 24 * 60 * 60)))
            .contains("3 days have passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(None)
            .contains("recreate timers or other session-scoped setup if still needed")
    );
}

/// Loading an existing transcript into a different session must warn the agent
/// once that operational extension state, especially timers, may not follow it.
#[test]
fn existing_agent_loaded_into_different_session_gets_session_state_notice() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent_id = tau_proto::AgentId::parse("existing-agent").expect("agent id");
    h.append_direct_agent_semantic_event(
        agent_id.as_str(),
        tau_core::AgentEventParent::InheritHead,
        Event::AgentStarted(tau_proto::AgentStarted {
            creator: Some(tau_proto::AgentCreator::default()),

            parent_agent: None,
            agent_id: agent_id.clone(),
            role: "engineer".to_owned(),
            display_name: None,
            metadata: Vec::new(),
            ephemeral: false,
        }),
    )
    .expect("seed existing agent");

    let cid = crate::parse_agent_id(agent_id.as_str());
    let mut agent = Agent::new(
        cid.clone(),
        1,
        h.session_runtime.current_session_id.clone(),
        tau_proto::PromptOriginator::User,
        None,
        None,
    );
    agent.identity.role = Some("engineer".to_owned());
    agent.identity.agent_id = Some(agent_id.clone());
    h.agent_runtime
        .agent_registry
        .agents
        .insert(cid.clone(), agent);
    h.ensure_loaded_agent_for_agent(&cid, &agent_id);

    assert!(
        !event_log_contains_any_source(&h, |event| matches!(event, Event::AgentPromptCreated(_))),
        "loading must not create a standalone notice turn"
    );
    h.submit_prompt_to_agent(
        h.session_runtime.current_session_id.clone(),
        agent_id.as_str(),
        "continue in this session".to_owned(),
    )
    .expect("submit user prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    let context = prompt.context.flatten();
    let notice = context
        .iter()
        .filter_map(text_part)
        .find(|text| text.contains("loaded into a different session"))
        .expect("changed-session notice in user turn");
    assert!(notice.contains("recreate timers"));
    assert!(
        h.take_pending_restore_prompts_for_user_prompt(&cid)
            .is_empty(),
        "changed-session notice must be one-shot"
    );

    h.shutdown().expect("shutdown");
}

/// Reloading an agent within the same durable session must not mislabel that
/// ordinary lifecycle transition as a move to another session.
#[test]
fn same_session_agent_reload_does_not_repeat_changed_session_notice() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.unload_agent_from_session_if_loaded(
        &h.session_runtime.current_session_id.clone(),
        agent_id.as_str(),
    );
    h.ensure_loaded_agent_for_agent(&cid, &agent_id);

    assert!(
        h.take_pending_restore_prompts_for_user_prompt(&cid)
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// Ephemeral session journals intentionally have no durable events, so runtime
/// membership history must still prevent false changed-session warnings.
#[test]
fn ephemeral_same_session_agent_reload_does_not_warn() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness_ephemeral(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.unload_agent_from_session_if_loaded(
        &h.session_runtime.current_session_id.clone(),
        agent_id.as_str(),
    );
    h.ensure_loaded_agent_for_agent(&cid, &agent_id);

    assert!(
        h.take_pending_restore_prompts_for_user_prompt(&cid)
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// A completed durable worker must not recover the transient start request as
/// the owner of future turns. This regression exercises a real tool-backed
/// start, cold restoration, and a fresh targeted turn so a stale extension
/// originator cannot emit a second result or unload the worker.
#[test]
fn cold_restored_completed_worker_is_ordinary_and_remains_loaded() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let worker_agent_id = {
        let mut h = echo_harness(&sp).expect("start");
        h.config.selected_model = Some("test/model".into());
        let _delegate_events = connect_test_tool(&mut h, "conn-cold-delegate");
        let parent_cid = ensure_test_user_agent(&mut h);
        let parent_agent_id = durable_agent_id_for_conversation(&h, &parent_cid);
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert("cold-delegate-call".into(), parent_cid);

        let mut query = ext_query("q-cold-completed");
        query.tool_call_id = Some("cold-delegate-call".into());
        h.handle_start_agent_request(&crate::test_connection_id("conn-cold-delegate"), query)
            .expect("start worker");
        let worker_cid = ext_query_cid(&h, "q-cold-completed").expect("worker conversation");
        let worker_agent_id = durable_agent_id_for_conversation(&h, &worker_cid);
        let worker_prompt_id = h
            .prompt_coordination
            .prompt_runtime
            .agents
            .iter()
            .find_map(|(prompt_id, cid)| (cid == &worker_cid).then_some(prompt_id.clone()))
            .expect("worker prompt");
        let mut completed = provider_text_response(
            &worker_prompt_id,
            worker_agent_id.clone(),
            "worker complete",
        );
        completed.originator = tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("conn-cold-delegate"),
            query_id: "q-cold-completed".to_owned(),
        };
        h.handle_provider_response_finished(completed)
            .expect("complete worker");

        let worker = h
            .agent_runtime
            .agent_registry
            .agents
            .get(&worker_cid)
            .expect("detached worker");
        assert!(worker.identity.originator.is_user());
        assert!(worker.identity.source_connection.is_none());
        assert!(worker.identity.parent_tool_call_id.is_none());
        assert_eq!(
            h.agent_runtime
                .agent_registry
                .navigation_modes
                .get(&worker_agent_id),
            Some(&tau_proto::AgentNavigationMode::ActiveAuto)
        );
        let creation = h
            .session_runtime
            .agent_store
            .agent_events(worker_agent_id.as_str())
            .expect("worker journal")
            .into_iter()
            .find_map(|record| match record.event {
                Event::AgentStarted(started) => Some(started),
                _ => None,
            })
            .expect("worker creation");
        assert_eq!(creation.parent_agent.as_ref(), Some(&parent_agent_id));
        h.shutdown().expect("shutdown first boot");
        worker_agent_id
    };

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    h.config.selected_model = Some("test/model".into());
    let worker_cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(worker_agent_id.as_str())
        .cloned()
        .expect("restored worker route");
    let worker = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&worker_cid)
        .expect("restored worker");
    assert!(worker.identity.originator.is_user());
    assert!(matches!(worker.turn.turn_state, AgentTurnState::Idle));
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&worker_agent_id),
        Some(&tau_proto::AgentNavigationMode::ActiveAuto)
    );

    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "fresh worker turn".to_owned(),
            agent_id: worker_agent_id.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("submit fresh worker turn");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&worker_agent_id),
        Some(&tau_proto::AgentNavigationMode::Active),
        "accepted direct UI input overrides the restored delegated default"
    );
    let fresh_prompt_id = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(prompt_id, cid)| (cid == &worker_cid).then_some(prompt_id.clone()))
        .expect("fresh worker prompt");
    assert!(
        read_prompt_created(&h, &fresh_prompt_id)
            .originator
            .is_user()
    );
    h.handle_provider_response_finished(provider_text_response(
        &fresh_prompt_id,
        worker_agent_id.clone(),
        "fresh worker response",
    ))
    .expect("finish fresh worker turn");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&worker_agent_id),
        Some(&tau_proto::AgentNavigationMode::Active),
        "the implicit write survives the turn becoming idle"
    );

    assert_eq!(
        h.agent_runtime
            .agent_registry
            .agent_routes
            .get(worker_agent_id.as_str()),
        Some(&worker_cid)
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .session_loaded
            .contains(&worker_agent_id)
    );
    assert!(matches!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&worker_cid)
            .expect("worker remains loaded")
            .turn
            .turn_state,
        AgentTurnState::Idle
    ));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::StartAgentResult(result) if result.query_id == "q-cold-completed"
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::SessionAgentUnloaded(unloaded) if unloaded.agent_id == worker_agent_id
    )));
    let session_events = h
        .session_runtime
        .store
        .session_events("s1")
        .expect("session events");
    assert_eq!(
        session_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::SessionAgentLoaded(loaded) if loaded.agent_id == worker_agent_id
            ))
            .count(),
        1
    );
    assert_eq!(
        session_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::SessionAgentUnloaded(unloaded) if unloaded.agent_id == worker_agent_id
            ))
            .count(),
        0
    );
    assert!(
        h.session_runtime
            .store
            .session("s1")
            .expect("folded session")
            .contains_agent(&worker_agent_id)
    );
    h.shutdown().expect("shutdown resumed boot");
    drop(h);

    let mut restored_again =
        echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
            .expect("second cold resume");
    assert_eq!(
        restored_again
            .agent_runtime
            .agent_registry
            .navigation_modes
            .get(&worker_agent_id),
        Some(&tau_proto::AgentNavigationMode::ActiveAuto),
        "cold replay must recompute the delegated default instead of replaying Active"
    );
    restored_again
        .shutdown()
        .expect("shutdown second cold resume");
}

/// A process can stop after the terminal provider event is durable but before
/// the warm completion path returns the result and detaches the request owner.
/// Explicit-parent typed starts have no tool-call id, so cold classification
/// must still converge on the retained ordinary-worker state.
#[test]
fn cold_restore_detaches_explicit_parent_worker_at_terminal_before_teardown_cut() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let worker_agent_id = {
        let mut h = echo_harness(&sp).expect("start");
        h.config.selected_model = Some("test/model".into());
        let parent_cid = ensure_test_user_agent(&mut h);
        let parent_agent_id = durable_agent_id_for_conversation(&h, &parent_cid);
        let mut query = ext_query("q-explicit-parent-cut");
        query.parent_agent = Some(parent_agent_id);
        h.handle_start_agent_request(&crate::test_connection_id(HARNESS_CONNECTION_ID), query)
            .expect("start explicit-parent worker");
        let worker_cid = ext_query_cid(&h, "q-explicit-parent-cut").expect("worker conversation");
        let worker_agent_id = durable_agent_id_for_conversation(&h, &worker_cid);
        let worker_prompt_id = h.agent_runtime.agent_registry.agents[&worker_cid]
            .dispatch
            .in_flight_prompt
            .clone()
            .expect("worker prompt");
        let mut terminal = provider_text_response(
            &worker_prompt_id,
            worker_agent_id.clone(),
            "worker complete",
        );
        terminal.originator = tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name(HARNESS_CONNECTION_ID),
            query_id: "q-explicit-parent-cut".to_owned(),
        };

        // Deliberately persist only the terminal provider fact. This models the
        // crash cut before side-conversation classification and reduction can
        // deliver StartAgentResult and detach the completed request owner.
        h.publish_finished_response_for_agent(&worker_cid, None, &terminal, None, false);
        assert!(matches!(
            h.agent_runtime.agent_registry.agents[&worker_cid]
                .identity
                .originator,
            tau_proto::PromptOriginator::Extension { .. }
        ));
        assert!(
            h.agent_runtime.agent_registry.agents[&worker_cid]
                .identity
                .parent_agent_id
                .is_some()
        );
        assert!(
            h.agent_runtime
                .agent_registry
                .session_loaded
                .contains(&worker_agent_id)
        );
        h.shutdown().expect("shutdown at terminal cut");
        worker_agent_id
    };

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume terminal cut");
    h.config.selected_model = Some("test/model".into());
    let worker_cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(worker_agent_id.as_str())
        .cloned()
        .expect("restored worker route");
    let worker = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&worker_cid)
        .expect("restored worker");
    assert!(worker.identity.originator.is_user());
    assert!(worker.identity.source_connection.is_none());
    assert!(worker.identity.parent_agent_id.is_none());
    assert!(worker.identity.parent_tool_call_id.is_none());
    assert!(matches!(worker.turn.turn_state, AgentTurnState::Idle));
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&worker_agent_id),
        Some(&tau_proto::AgentNavigationMode::ActiveAuto)
    );

    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "fresh after terminal cut".to_owned(),
            agent_id: worker_agent_id.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("submit fresh worker turn");
    let fresh_prompt_id = h.agent_runtime.agent_registry.agents[&worker_cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("fresh worker prompt");
    h.handle_provider_response_finished(provider_text_response(
        &fresh_prompt_id,
        worker_agent_id.clone(),
        "fresh response",
    ))
    .expect("complete fresh worker turn");

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&worker_cid)
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .session_loaded
            .contains(&worker_agent_id)
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::StartAgentResult(result) if result.query_id == "q-explicit-parent-cut"
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::SessionAgentUnloaded(unloaded) if unloaded.agent_id == worker_agent_id
    )));
    h.shutdown().expect("shutdown resumed worker");
}

/// Reactive context recovery is an explicit continuation, not evidence that a
/// parented start request completed. A cold resume must retain the prior
/// extension classification through the existing interrupted-recovery path.
#[test]
fn cold_restore_does_not_classify_reactive_recovery_as_completed_worker() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let worker_agent_id = {
        let mut h = quiet_provider_harness(&sp).expect("start");
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .supports_standalone_compaction = true;
        let parent_cid = ensure_test_user_agent(&mut h);
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert("reactive-cut-call".into(), parent_cid);
        let mut query = ext_query("q-reactive-cut");
        query.tool_call_id = Some("reactive-cut-call".into());
        h.handle_start_agent_request(&crate::test_connection_id(HARNESS_CONNECTION_ID), query)
            .expect("start worker");
        let worker_cid = ext_query_cid(&h, "q-reactive-cut").expect("worker");
        let worker_agent_id = durable_agent_id_for_conversation(&h, &worker_cid);
        let inference = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(context_overflow_response(&inference))
            .expect("start reactive recovery");
        assert_eq!(
            read_nth_prompt_created(&h, 1).operation,
            tau_proto::PromptOperation::StandaloneCompaction
        );
        assert!(matches!(
            h.agent_runtime.agent_registry.agents[&worker_cid]
                .identity
                .originator,
            tau_proto::PromptOriginator::Extension { .. }
        ));
        h.shutdown().expect("shutdown reactive cut");
        worker_agent_id
    };

    let mut cold_reader =
        echo_harness_for("classification-only", &sp).expect("open cold journal reader");
    let originator = cold_reader
        .restored_agent_runtime_from_log(worker_agent_id.as_str())
        .originator;
    assert!(matches!(
        originator,
        tau_proto::PromptOriginator::Extension { .. }
    ));
    cold_reader.shutdown().expect("shutdown cold reader");
}

/// Cold reconstruction must use the latest committed prompt authority rather
/// than permanently inheriting the extension query that created an endpoint.
#[test]
fn cold_restore_uses_latest_committed_prompt_originator() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let worker_agent_id = {
        let mut h = quiet_provider_harness(&sp).expect("start");
        h.handle_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query("q-adopted-before-restart"),
        )
        .expect("start extension endpoint");
        let worker_cid = ext_query_cid(&h, "q-adopted-before-restart").expect("extension endpoint");
        let worker_agent_id = durable_agent_id_for_conversation(&h, &worker_cid);

        h.publish_pending_prompt_for_agent(
            &worker_cid,
            PendingPrompt::human_ui("adopt endpoint".to_owned()),
        )
        .expect("commit authenticated user authority");
        assert!(event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::AgentPromptSubmitted(prompt)
                if prompt.agent_id == worker_agent_id
                    && prompt.originator == tau_proto::PromptOriginator::User
                    && prompt.submission_source
                        == tau_proto::PromptSubmissionSource::HumanUi
        )));
        h.shutdown().expect("shutdown adopted endpoint");
        worker_agent_id
    };

    let mut cold_reader =
        echo_harness_for("classification-only", &sp).expect("open cold journal reader");
    assert_eq!(
        cold_reader
            .restored_agent_runtime_from_log(worker_agent_id.as_str())
            .originator,
        tau_proto::PromptOriginator::User
    );
    cold_reader.shutdown().expect("shutdown cold reader");
}

/// Model-visible text cannot spoof harness-owned passive provenance: a normal
/// activating prompt remains active even when its bytes resemble a restore
/// notice.
#[test]
fn restore_looking_user_text_remains_an_activation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let text = crate::harness::restore_notice_prompt_for_elapsed(None);

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::human_ui(text.clone()))
        .expect("dispatch spoof-shaped user text");

    assert!(event_log_events(&h).into_iter().any(|event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text == text && submitted.inference_activation
    )));
    h.shutdown().expect("shutdown");
}

/// A completed checkpoint consumes every true activation through its head,
/// leaves a later true node pending once, and an uncompleted replacement
/// checkpoint restores as uncertain without redispatch. This guards
/// `SPEC-compaction-and-context-recovery`.
#[test]
fn replay_respects_activation_checkpoint_ranges_and_uncertainty() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    seed_main_agent_loaded(&state);
    let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
    let mut store = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
    for text in ["activation A", "activation B"] {
        append_seed_agent_event(
            &mut store,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: true,
                agent_id: agent_id.clone(),
                text: text.to_owned(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        );
    }
    let through = store
        .agent("main")
        .and_then(tau_core::AgentTree::head)
        .expect("second activation head");
    let completed_prompt_id = test_agent_prompt_id("ap-main-completed");
    append_seed_agent_event(
        &mut store,
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            output_length_continuation: None,
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: completed_prompt_id.clone(),
            through: tau_proto::AgentHead::Node(through),
            model: None,
            operation: None,
            activation_cut: None,
        }),
    );
    append_seed_agent_event(
        &mut store,
        Event::ProviderResponseFinished(provider_text_response(
            &completed_prompt_id,
            agent_id.clone(),
            "completed response",
        )),
    );
    append_seed_agent_event(
        &mut store,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: true,
            agent_id,
            text: "activation C".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    drop(store);

    let uncertain_prompt_id = {
        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("first resume");
        let prompt = read_nth_prompt_created(&h, 0);
        let uncertain_prompt_id = prompt.agent_prompt_id.clone();
        let context = serde_json::to_string(&prompt.context).expect("context");
        assert_eq!(context.matches("activation C").count(), 1);
        assert_eq!(
            event_log_events(&h)
                .into_iter()
                .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
                .count(),
            1,
            "later activation should be checkpointed exactly once"
        );
        h.shutdown().expect("shutdown");
        uncertain_prompt_id
    };

    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("uncertain resume");
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(_) | Event::AgentInferenceDispatchStarted(_)
    )));
    let uncertain = h
        .agent_runtime
        .agent_watch
        .provider_status
        .get("main")
        .expect("restored uncertain watcher snapshot");
    assert!(matches!(
        uncertain.state,
        tau_proto::AgentWatchProviderState::DispatchUncertain {
            category: tau_proto::AgentWatchProviderCategory::Unknown
        }
    ));
    assert_eq!(uncertain.agent_prompt_id, uncertain_prompt_id);
    h.shutdown().expect("shutdown");
}

/// Dispatching work to a loaded durable agent refreshes its session retention
/// hint while preserving the manifest's canonical creation timestamp.
#[test]
fn durable_agent_dispatch_extends_session_retention() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let session_id = h.session_runtime.current_session_id.clone();
    let meta_path = h
        .session_runtime
        .store
        .sessions_dir()
        .join(session_id.as_str())
        .join("meta.json");
    path_std_fs::write(
        &meta_path,
        serde_json::to_vec(&tau_core::SessionMeta {
            created_at: 7,
            last_touched: 8,
        })
        .expect("encode stale manifest"),
    )
    .expect("write stale manifest");

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("operational use".to_owned()))
        .expect("dispatch prompt");

    assert_session_manifest_refreshed(&meta_path);
    h.shutdown().expect("shutdown");
}

/// Initial durable-session setup commits canonical existence before enabling
/// session diagnostics, so a manifest failure leaves only lock scaffolding.
#[test]
fn failed_initial_manifest_prevents_session_diagnostic_creation() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = echo_harness(&state).expect("harness");
    let sessions_dir = tau_config::settings::sessions_dir_of(&state);
    let session_dir = sessions_dir.join("blocked-manifest");
    path_std_fs::create_dir_all(session_dir.join("meta.json"))
        .expect("obstruct canonical manifest path");

    h.prepare_initial_session_storage(
        &sessions_dir,
        "blocked-manifest",
        path_std_time::Instant::now(),
    )
    .expect_err("manifest obstruction must fail session setup");

    assert!(!session_dir.join("events.jsonl").exists());
    assert!(!session_dir.join("lock").exists());
    h.shutdown().expect("shutdown");
}

/// A pending replay activation for a loaded durable agent refreshes session
/// retention even though it bypasses ordinary prompt submission.
#[test]
fn durable_replay_activation_extends_session_retention() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("loaded durable agent")
        .dispatch
        .pending_replay_activation = true;
    let meta_path = stale_session_manifest(&h);
    let capture = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(TraceCaptureLayer {
        capture: capture.clone(),
    });
    tracing::subscriber::with_default(subscriber, || h.try_advance_queue());

    assert_session_manifest_refreshed(&meta_path);
    assert!(
        capture
            .events
            .lock()
            .expect("trace capture lock")
            .is_empty(),
        "replay activation session activity must not enter prompt-acceptance tracing"
    );
    h.shutdown().expect("shutdown");
}
