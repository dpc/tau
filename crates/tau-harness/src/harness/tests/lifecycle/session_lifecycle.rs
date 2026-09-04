//! Tests for session lifecycle behavior.

use super::*;

/// Ensures stale or malformed shell finish events cannot inject output into the
/// wrong session when an explicit target agent belongs to another session.
#[test]
fn shell_output_for_wrong_session_is_ignored() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.inject_user_shell_output(&tau_proto::ShellCommandFinished {
        command_id: tau_proto::ShellCommandId::parse("shell-2")
            .expect("test identifier must satisfy its grammar"),
        session_id: test_session_id("other-session"),
        command: "printf wrong".to_owned(),
        include_in_context: true,
        target_agent_id: Some(agent_id),
        output: "wrong".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    });

    assert!(loaded_agent_events(&h, "s1").into_iter().all(|event| {
        !matches!(
            event,
            Event::AgentUserMessageInjected(injected)
                if injected.text.contains("printf wrong")
        )
    }));
}

#[test]
fn resumed_session_init_does_not_reinject_agents_context() {
    // Regression: cold resume must wait for extensions to refresh their
    // context, but the restored conversation already contains the startup
    // AGENTS.md user message. Appending it again makes the model see a
    // duplicate user instruction before the first resumed prompt.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_connection_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let marker = "resume AGENTS marker";
    let count_marker_injections = |h: &Harness| -> usize {
        loaded_agent_events(h, "s1")
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    Event::AgentUserMessageInjected(injected)
                        if injected.text.contains(marker)
                )
            })
            .count()
    };

    h.prompt_coordination.context_discovery.agents_files.clear();
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");
    h.publish_event_for_agent(
        &cid,
        None,
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            inference_activation: false,
            agent_id: crate::parse_agent_id(&agent_id),
            text: format!("# AGENTS.md instructions\n{marker}"),
            message_class: tau_proto::PromptMessageClass::User,
        }),
    );
    assert_eq!(count_marker_injections(&h), 1);

    h.prompt_coordination
        .context_discovery
        .agents_files
        .push(DiscoveredAgentsFile {
            source_id: crate::test_connection_id(tools_connection_id.clone()),
            file_path: PathBuf::from("/repo/AGENTS.md"),
            content: format!("# Root\n- {marker}\n"),
        });
    h.prompt_coordination
        .pending_notices
        .restore_sessions
        .insert(test_session_id("s1"), None);
    h.session_runtime.turn_state = TurnState::InitializingSession {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Resume,
        waiting_on: [crate::test_connection_id(tools_connection_id.clone())]
            .into_iter()
            .collect(),
    };
    h.handle_extension_event(
        &tools_connection_id,
        TestProtocolItem::Event(Event::ExtensionSessionContextReady(
            tau_proto::ExtensionSessionContextReady {
                session_id: test_session_id("s1"),
            },
        )),
    )
    .expect("ready");

    assert!(matches!(h.session_runtime.turn_state, TurnState::Idle));
    assert_eq!(count_marker_injections(&h), 1);
    assert!(
        h.prompt_coordination
            .pending_notices
            .restore_sessions
            .contains_key("s1"),
        "restore notice queue should be independent from AGENTS.md injection"
    );

    h.shutdown().expect("shutdown");
}

/// A matching expected session receives an explicit admission acknowledgement
/// so the UI can verify binding before it starts rendering.
#[test]
fn client_hello_acknowledges_matching_expected_session() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let events = connect_test_client(&mut h, "attach-ui", tau_proto::ClientKind::Ui);
    let expected = h.session_runtime.current_session_id.clone();

    let keep = h
        .handle_client_event(
            "attach-ui",
            TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: crate::test_extension_name("attach-ui"),
                client_kind: tau_proto::ClientKind::Ui,
                expected_session_id: Some(expected.clone()),
                capabilities: Default::default(),
            })),
        )
        .expect("matching session is admitted");

    assert!(keep);
    let events = events.lock().expect("events");
    assert!(events.iter().any(|event| matches!(
        &event.frame,
        HarnessOutputMessage::SessionAccepted(accepted)
            if accepted.session_id == expected
    )));
}

/// A persisted fourth-attempt successor Length restores one initial
/// TerminalIncomplete notification for a cold late watcher without replaying
/// the response as live activity.
#[test]
fn output_length_attempt_four_restores_late_watcher_terminal_incomplete() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish twice".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 11))
        .expect("source length response");
    let successor = read_nth_prompt_created(&h, 1);
    let source_cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(source.agent_id.as_str())
        .cloned()
        .expect("source route");
    h.report_agent_work_status(
        &source_cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "bounded work".to_owned(),
        )
        .expect("working report"),
    )
    .expect("record working status");
    let public_id = durable_agent_id_for_conversation(&h, &source_cid);
    let live_watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let live_watcher_id = durable_agent_id_for_conversation(&h, &live_watcher_cid).to_string();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&live_watcher_cid)
        .expect("live watcher")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("live-late-watcher"),
    };
    let turn_generation = h.agent_runtime.agent_registry.agents[&source_cid]
        .turn
        .turn_generation;
    h.agent_runtime.agent_watch.provider_status.insert(
        public_id.to_string(),
        tau_proto::AgentWatchProviderStatusNotification {
            session_id: test_session_id("s1"),
            subscription_id: String::new(),
            turn_generation,
            agent_prompt_id: successor.agent_prompt_id.clone(),
            state: tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Transport,
                attempt: 3,
                next_retry_delay_secs: 0,
            },
            initial: false,
        },
    );
    let mut successor_response = reasoning_only_length_response(&successor, 13);
    successor_response.provider_attempt =
        tau_proto::ProviderAttempt::new(99).expect("nonzero attempt");
    h.handle_provider_response_finished(successor_response)
        .expect("successor length response");

    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable events");
    let dispositions = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(response) => {
                Some(response.output_length_disposition.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(dispositions.len(), 2);
    assert!(matches!(
        dispositions[0],
        tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
    ));
    assert!(matches!(
        dispositions[1],
        tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outcome: tau_proto::OutputLengthContinuationOutcome::Incomplete,
            outer_turn_finish_owed: true,
            ..
        }
    ));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2
    );
    assert_eq!(
        h.session_runtime
            .current_session_state
            .token_usage
            .total
            .received_tokens,
        24
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&source_cid]
            .turn
            .work_status
            .phase(),
        tau_proto::AgentWorkStatusPhase::Unknown
    );
    assert!(matches!(
        h.agent_runtime
            .agent_watch
            .provider_status
            .get(public_id.as_str())
            .map(|status| &status.state),
        Some(tau_proto::AgentWatchProviderState::TerminalIncomplete {
            category: tau_proto::AgentWatchProviderCategory::OutputLength,
            attempt: 4,
        })
    ));
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentMessageReceived(message)
            if message.kind == tau_proto::AgentMessageKind::WatchResponse
    )));
    assert!(
        h.agent_runtime
            .agent_watch
            .provider_status
            .contains_key(public_id.as_str())
    );
    h.try_set_agent_watch(
        &live_watcher_id,
        public_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    )
    .expect("enable live late watcher");
    let live_events = event_log_events(&h);
    let live_late = live_events
        .iter()
        .filter_map(|event| match event {
            Event::AgentMessageReceived(message)
                if message.recipient_id.as_str() == live_watcher_id
                    && message.kind == tau_proto::AgentMessageKind::WatchProviderStatus =>
            {
                message.watch_provider_status.as_ref()
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        live_late.len(),
        1,
        "pending_intercept={}, deferred={}, publish_idle={}",
        h.runtime_io.publication.pending_intercept.is_some(),
        h.runtime_io.publication.deferred.len(),
        h.runtime_io.publication.idle_dispatches.len(),
    );
    assert!(live_late[0].initial);
    assert!(matches!(
        live_late[0].state,
        tau_proto::AgentWatchProviderState::TerminalIncomplete {
            category: tau_proto::AgentWatchProviderCategory::OutputLength,
            attempt: 4,
        }
    ));
    h.shutdown().expect("shutdown");
    drop(h);
    let mut restored =
        echo_harness_with_start_reason("s1", td.path(), tau_proto::SessionStartReason::Resume)
            .expect("cold restore");
    assert!(matches!(
        restored
            .agent_runtime
            .agent_watch
            .provider_status
            .get(public_id.as_str())
            .map(|status| &status.state),
        Some(tau_proto::AgentWatchProviderState::TerminalIncomplete {
            category: tau_proto::AgentWatchProviderCategory::OutputLength,
            attempt: 4,
        })
    ));
    let startup_events = event_log_events(&restored);
    assert!(!startup_events.iter().any(|event| matches!(
        event,
        Event::ProviderResponseFinished(response)
            if response.agent_prompt_id == successor.agent_prompt_id
    )));
    let watcher_cid = restored.create_durable_user_agent(
        restored.session_runtime.current_session_id.clone(),
        &restored.config.selected_role.clone(),
    );
    let watcher_id = durable_agent_id_for_conversation(&restored, &watcher_cid).to_string();
    restored.set_agent_watch(
        &watcher_id,
        public_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let restored_events = event_log_events(&restored);
    let late = restored_events
        .iter()
        .filter_map(|event| match event {
            Event::AgentMessageReceived(message)
                if message.recipient_id.as_str() == watcher_id
                    && message.kind == tau_proto::AgentMessageKind::WatchProviderStatus =>
            {
                message.watch_provider_status.as_ref()
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(late.len(), 1);
    assert!(late[0].initial);
    assert!(matches!(
        late[0].state,
        tau_proto::AgentWatchProviderState::TerminalIncomplete {
            category: tau_proto::AgentWatchProviderCategory::OutputLength,
            attempt: 4,
        }
    ));
    assert!(!event_log_events(&restored).iter().any(|event| matches!(
        event,
        Event::ProviderResponseFinished(response)
            if response.agent_prompt_id == successor.agent_prompt_id
    )));
    restored.shutdown().expect("restored shutdown");
}

#[test]
fn startup_diagnostics_are_mandatory_warning_and_replayed() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");

    h.emit_extension_startup_diagnostics(&[crate::settings::ExtensionStartupDiagnostic {
        extension: "optional-diagnostic".to_owned(),
        message: "optional extension optional-diagnostic did not initialize".to_owned(),
        kind: ExtensionStartupDiagnosticKind::OptionalSkip,
    }]);

    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                    && info.purpose == tau_proto::NoticePurpose::Alert
                    && info.message == "optional extension optional-diagnostic did not initialize"
        )
    ));

    let ui_conn: tau_proto::ConnectionId = crate::test_connection_id("late-ui-startup-diagnostic");
    let ui_sink = connect_test_client(&mut h, ui_conn.as_str(), tau_proto::ClientKind::Ui);
    h.handle_client_event(
        &ui_conn,
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Prefix("harness.".to_owned())],
        })),
    )
    .expect("subscribe");
    let frames = ui_sink.lock().expect("ui sink");
    assert!(frames.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::HarnessNotice(info))
            if info.level == tau_proto::NoticeLevel::Warning
                && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                && info.purpose == tau_proto::NoticePurpose::Alert
                && info.message == "optional extension optional-diagnostic did not initialize"
    )));
}

#[test]
fn session_init_catchup_replays_current_session_dir_to_early_subscribers() {
    // Regression coverage for configured extensions that subscribe during
    // startup after the live `harness.session_dir` notice but before the session
    // is marked initialized. Init completion must replay the current-state
    // session-dir snapshot so extensions can apply the correct persistence
    // policy.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let events = connect_test_client(&mut h, "early-session-dir", tau_proto::ClientKind::Provider);
    h.prompt_coordination
        .context_discovery
        .initialized_sessions
        .remove(&test_session_id("s1"));

    h.handle_extension_message(
        &crate::test_connection_id("early-session-dir"),
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::HARNESS_SESSION_DIR,
            )],
        }),
    )
    .expect("subscribe during init");
    assert!(
        events.lock().expect("events").is_empty(),
        "subscribe-time catch-up is skipped while session initialization is incomplete"
    );

    h.catch_up_subscribers_after_session_init();

    assert!(
        events.lock().expect("events").iter().any(|frame| matches!(
            &frame.frame,
            HarnessOutputMessage::Deliver(delivery)
                if matches!(delivery.event.as_ref(), Event::HarnessSessionDir(_))
        )),
        "session init catch-up should deliver current harness.session_dir"
    );

    h.shutdown().expect("shutdown");
}

/// Ensures the session-init catch-up path does not replay startup status
/// snapshots to an already-attached UI. The initial terminal UI subscribes
/// before startup publishes `harness.session_dir` and `extension.ready`, so
/// replaying the same current-state snapshot at init completion visibly
/// duplicates the startup status block.
#[test]
fn session_init_catchup_does_not_duplicate_ui_startup_status_snapshots() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let events = connect_test_client(&mut h, "startup-ui", tau_proto::ClientKind::Ui);
    let selectors = vec![
        tau_proto::EventSelector::Exact(tau_proto::EventName::HARNESS_SESSION_DIR),
        tau_proto::EventSelector::Exact(tau_proto::EventName::EXTENSION_READY),
    ];
    h.prompt_coordination
        .context_discovery
        .initialized_sessions
        .remove(&test_session_id("s1"));

    h.handle_client_message(
        &crate::test_connection_id("startup-ui"),
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: selectors.clone(),
        })
        .into_input_message(),
    )
    .expect("subscribe during init");
    assert!(
        events.lock().expect("events").is_empty(),
        "subscribe-time catch-up is skipped while session initialization is incomplete"
    );

    h.replay_harness_notice(&crate::test_connection_id("startup-ui"), &selectors);
    h.catch_up_subscribers_after_session_init();

    let events = events.lock().expect("events");
    let session_dir_count = events
        .iter()
        .filter(|frame| {
            matches!(
                peel_inner_event(&frame.frame),
                Some(Event::HarnessSessionDir(_))
            )
        })
        .count();
    let extension_ready_count = events
        .iter()
        .filter(|frame| {
            matches!(
                peel_inner_event(&frame.frame),
                Some(Event::ExtensionReady(_))
            )
        })
        .count();
    assert_eq!(
        session_dir_count, 1,
        "startup UI should see one session-dir status, not live plus catch-up duplicates"
    );
    assert_eq!(
        extension_ready_count, 1,
        "startup UI should see one extension-ready status, not live plus catch-up duplicates"
    );
    drop(events);

    h.shutdown().expect("shutdown");
}
