//! Tests for extension lifecycle behavior.

use super::super::super::{MAX_EXTENSION_ACTIVATION_BYTES, MAX_EXTENSION_ACTIVATION_MESSAGES};
use super::super::dispatch::provider_text_response;
use super::*;
use crate::event::{HarnessCommand, ShutdownCause};
use crate::harness::STARTUP_TIMEOUT;
use crate::harness::extension_activation::{
    ToolStartedSubscriptionWork, reset_tool_started_subscription_work,
    tool_started_subscription_work,
};

/// Session rollover resets only budget exhaustion; a peer disabled by
/// configuration policy remains disabled.
#[test]
fn session_restart_budget_reset_preserves_permanent_disablement() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    for connection_id in ["budget-disabled", "permanently-disabled"] {
        let _sink = connect_handshaking_tool(&mut h, connection_id);
        let entry = h
            .extensions
            .entries
            .get_mut(connection_id)
            .expect("extension");
        entry.state = ExtensionState::Disconnected;
        entry.respawn_allowed = false;
        entry.restart_attempt = MAX_EXTENSION_RESTART_ATTEMPTS;
        entry.supervised_config = Some(supervised_test_config(connection_id, "exit 1"));
    }
    h.extensions
        .restart_budget_disabled
        .insert(crate::test_connection_id("budget-disabled"));
    let rollover_at = Instant::now();

    h.reset_extension_restart_budgets_at(rollover_at);

    let budget_disabled = &h.extensions.entries["budget-disabled"];
    assert!(budget_disabled.respawn_allowed);
    assert_eq!(budget_disabled.restart_attempt, 0);
    assert_eq!(
        h.extensions.restart_deadlines["budget-disabled"],
        rollover_at + EXTENSION_RESTART_DELAY
    );
    let permanently_disabled = &h.extensions.entries["permanently-disabled"];
    assert!(!permanently_disabled.respawn_allowed);
    assert_eq!(permanently_disabled.restart_attempt, 0);
    assert!(
        !h.extensions
            .restart_deadlines
            .contains_key("permanently-disabled")
    );
    h.shutdown().expect("shutdown");
}

/// Supervised providers preserve fatal/non-respawn policy even though their
/// writer cleanup uses the same retained ownership path.
#[test]
fn crashing_supervised_provider_never_schedules_restart() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    connect_supervised_test_process(
        &mut h,
        supervised_test_config("crash-provider", "exit 1"),
        tau_proto::ClientKind::Provider,
    );

    let connection_id = drive_crashed_extension_cleanup(&mut h, "crash-provider", Instant::now());
    let entry = &h.extensions.entries[&connection_id];
    assert_eq!(entry.restart_attempt, 0);
    assert!(!h.extensions.restart_deadlines.contains_key(&connection_id));
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::ExtensionRestarting(_)))
    );
    h.shutdown().expect("shutdown");
}

/// A UI that reaches a stale or replaced socket must be rejected before it can
/// subscribe to a session other than the explicit attach target.
#[test]
fn client_hello_rejects_expected_session_mismatch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let events = connect_test_client(&mut h, "attach-ui", tau_proto::ClientKind::Ui);
    let requested = tau_proto::SessionId::parse("different-session").expect("valid session id");

    let keep = h
        .handle_client_event(
            "attach-ui",
            TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: crate::test_extension_name("attach-ui"),
                client_kind: tau_proto::ClientKind::Ui,
                expected_session_id: Some(requested.clone()),
                capabilities: Default::default(),
            })),
        )
        .expect("mismatched session is a client-local rejection");

    assert!(!keep);
    let events = events.lock().expect("events");
    assert!(events.iter().any(|event| matches!(
        &event.frame,
        HarnessOutputMessage::Disconnect(disconnect)
            if disconnect.reason.as_deref().is_some_and(|reason|
                reason.contains(requested.as_str())
                && reason.contains(h.session_runtime.current_session_id.as_str()))
    )));
}

#[test]
fn optional_startup_timeout_is_mandatory_warning_replayed_and_nonfatal() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "optional-timeout-ext";
    let _extension_sink = connect_handshaking_tool(&mut h, conn_id);
    h.extensions
        .entries
        .get_mut(conn_id)
        .expect("extension entry")
        .require = false;

    h.handle_extensions_startup_timeout()
        .expect("only optional blockers should not fail startup");

    let entry = h.extensions.entries.get(conn_id).expect("extension entry");
    assert_eq!(entry.state, ExtensionState::Disconnected);
    assert!(!entry.respawn_allowed);
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                    && info.purpose == tau_proto::NoticePurpose::Alert
                    && info.message == "optional extension optional-timeout-ext did not initialize"
        )
    ));

    let ui_conn: tau_proto::ConnectionId = crate::test_connection_id("late-ui-timeout");
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
                && info.message == "optional extension optional-timeout-ext did not initialize"
    )));
}

#[test]
fn prompt_created_waits_for_registered_agent_context_provider() {
    // Context readiness is an explicit extension capability, not a side effect
    // of subscribing to `session.agent_loaded`. Once a provider registers, the
    // submitted user message may commit immediately, but `AgentPromptCreated`
    // must wait for that provider's per-agent context before freezing the model
    // snapshot.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let conn_id = "conn-agent-context-ready";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
        }),
    )
    .expect("subscribe");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtensionContextProviderRegister(
            tau_proto::ExtensionContextProviderRegister {},
        )),
    )
    .expect("register context provider");
    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtPromptFragmentPublish(
            tau_proto::ExtPromptFragmentPublish {
                fragment: tau_proto::PromptFragment::new(
                    "test.cwd",
                    tau_proto::PromptPriority::new(20),
                    "Current working directory: {{#each agent_context.cwd}}{{#if @first}}{{value}}{{/if}}{{/each}}",
                ),
            },
        )),
    )
    .expect("prompt fragment");

    h.dispatch_user_prompt(test_session_id("s1"), "first prompt".to_owned())
        .expect("dispatch user prompt");
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptSubmitted(prompt) if prompt.text == "first prompt"
    )));

    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .values()
        .find(|agent| agent.identity.originator.is_user())
        .and_then(|agent| agent.identity.agent_id.as_deref())
        .expect("durable user agent")
        .to_owned();
    let initialization_id = h.prompt_coordination.context_discovery.pending_agents
        [&crate::parse_agent_id(&agent_id)]
        .initialization_id
        .clone();
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtAgentContextPublish(
            tau_proto::ExtAgentContextPublish {
                session_id: test_session_id("s1"),
                agent_initialization_id: initialization_id.clone(),

                agent_id: crate::parse_agent_id(&agent_id),
                key: "cwd".into(),
                value: tau_proto::AgentContextValue(serde_json::json!("/tmp/work")),
            },
        )),
    )
    .expect("publish cwd");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                agent_initialization_id: initialization_id,

                session_id: test_session_id("s1"),
                agent_id: crate::parse_agent_id(&agent_id),
            },
        )),
    )
    .expect("context ready");

    let prompt = read_nth_prompt_created(&h, 0);
    assert!(prompt_context_contains(&prompt, "first prompt"));
    assert!(
        prompt
            .system_prompt
            .contains("Current working directory: /tmp/work")
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn disconnected_tool_completes_pending_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let call_id: ToolCallId = "call-1".into();
    let tool_name = ToolName::new("shell");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: test_agent_prompt_id("sp-main"),
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: vec![ContextItem::ToolCall(ToolCallItem {
                call_id: call_id.clone(),
                name: tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: tool_name.clone(),
            internal_name: tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .pending_tool_providers
        .insert(call_id.clone(), crate::test_connection_id(conn_id.clone()));
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(cid.clone(), call_id.clone(), ToolTurnCategories::default());
    if let Some(conv) = h.agent_runtime.agent_registry.agents.get_mut(&cid) {
        conv.turn.turn_state = AgentTurnState::ToolsRunning {
            remaining_calls: vec![call_id.clone()],
        };
    }

    h.handle_disconnect(&crate::test_connection_id(&conn_id));

    // Disconnect publishes a ToolError, drops the call from the
    // conversation's `ToolsRunning` set, and — since that was the
    // last outstanding call — re-prompts the agent so it can react
    // to the failure. The conversation therefore transitions
    // `ToolsRunning -> AgentThinking`, not back to `Idle`.
    assert!(matches!(h.session_runtime.turn_state, TurnState::Idle));
    assert!(matches!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&test_user_agent(&h))
            .expect("default conversation")
            .turn
            .turn_state,
        AgentTurnState::AgentThinking { .. }
    ));
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&call_id)
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key(&call_id)
    );

    let expected = extension_disconnected_tool_call_error_message(&call_id);
    assert!(default_agent_tree(&h).nodes().iter().any(|node| {
        matches!(
            &node.entry,
            AgentEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id == call_id
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message }
                                if message == &expected
                        )
                })
        )
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn disconnected_tool_is_removed_cleanly() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Send disconnect to the extension via the bus (through the
    // writer channel → writer thread → stream).
    let _ = h.runtime_io.bus.send_to(
        &crate::test_connection_id(&conn_id),
        None,
        HarnessOutputMessage::Disconnect(Disconnect {
            reason: Some("test".to_owned()),
        }),
    );

    // Drive event loop until the disconnect arrives.
    let started = Instant::now();
    loop {
        let event = h.expand_component_ingress_wake(
            h.runtime_io
                .rx
                .recv_timeout(Duration::from_secs(2))
                .expect("should get disconnect"),
        );
        match event {
            HarnessEvent::Disconnected {
                ref connection_id, ..
            } if *connection_id == conn_id => {
                h.handle_disconnect(&crate::test_connection_id(&conn_id));
                break;
            }
            HarnessEvent::FromConnection {
                connection_id,
                message,
                ..
            } => {
                let _ = h.handle_extension_message(&connection_id, *message);
            }
            _ => {}
        }
        assert!(started.elapsed() < Duration::from_secs(2), "timeout");
    }

    assert!(
        h.runtime_io
            .bus
            .connection(&crate::test_connection_id(&conn_id))
            .is_none()
    );
    assert!(h.tool_routing.registry.providers_for("shell").is_empty());
    assert!(
        h.session_runtime
            .lifecycle_messages
            .iter()
            .any(|m| m == "extension shell exited")
    );

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-x"), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "shell printf hi".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-x"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("removed tool should be rejected cleanly");

    let expected_prefix = unavailable_tool_error_message(&ToolName::new("shell"));
    assert!(default_agent_tree(&h).nodes().iter().any(|node| {
        matches!(
            &node.entry,
            AgentEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message } if message.starts_with(&expected_prefix)
                        )
                })
        )
    }));
    h.shutdown().expect("shutdown");
}

#[test]
fn extension_connect_command_installs_state_before_reader_ack() {
    // Regression: extension spawn helpers used to mutate bus state directly.
    // The reader must stay gated until the harness loop has installed both
    // the bus connection and the lifecycle entry, then emitted the starting
    // barrier.
    fn eager_hello_extension(r: UnixStream, w: UnixStream) -> Result<(), String> {
        let mut writer = TestInputWriter::new(BufWriter::new(w));
        writer
            .write_frame(&TestProtocolItem::Message(TestMessage::Hello(
                tau_proto::Hello {
                    protocol_version: tau_proto::PROTOCOL_VERSION,
                    client_name: crate::test_extension_name("late-tool"),
                    client_kind: tau_proto::ClientKind::Tool,
                    expected_session_id: None,
                    capabilities: Default::default(),
                },
            )))
            .map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        writer
            .write_frame(&TestProtocolItem::Message(TestMessage::Ready(
                tau_proto::Ready { message: None },
            )))
            .map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;

        let mut reader = TestOutputReader::new(BufReader::new(r));
        while let Some(frame) = reader.read_frame().map_err(|e| e.to_string())? {
            let frame = frame.into_event_frame();
            if matches!(frame, TestProtocolItem::Message(TestMessage::Disconnect(_))) {
                break;
            }
        }
        Ok(())
    }

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");

    let spawned = spawn_in_process(
        "late-tool",
        tau_proto::ClientKind::Tool,
        eager_hello_extension,
        &h.runtime_io.tx,
        &h.runtime_io.component_ingress_tx,
    )
    .expect("spawn late tool");
    let conn_id = spawned.connection_id.clone();
    h.queue_extension_connect(ExtensionConnectCommand {
        entry: ExtensionEntry {
            tool_prefix: None,
            name: crate::test_extension_name("late-tool"),
            instance_id: 999.into(),
            connection_id: conn_id.clone(),
            kind: tau_proto::ClientKind::Tool,
            peer_capabilities: Default::default(),
            pid: Some(std::process::id()),
            in_process_thread: Some(spawned.thread),
            supervised_config: None,
            secrets: BTreeMap::new(),
            require: true,
            respawn_allowed: true,
            restart_attempt: 0,
            state: ExtensionState::Spawning,
            protocol_io: spawned.protocol_io,
        },
        origin: ConnectionOrigin::Supervised,
        writer_tx: spawned.writer_tx,
        initialized_ack: spawned.initialized_ack,
        supervised_writer: None,
        replaces: None,
    })
    .expect("queue connect command");

    assert!(h.runtime_io.bus.connection(&conn_id).is_none());
    assert!(!h.extensions.entries.contains_key(&conn_id));

    let event = h.expand_component_ingress_wake(
        h.runtime_io
            .rx
            .recv_timeout(Duration::from_secs(1))
            .expect("connect command should be first"),
    );
    match event {
        HarnessEvent::Command(command) => h.handle_harness_command(command).expect("handle"),
        HarnessEvent::FromConnection { .. }
        | HarnessEvent::Disconnected { .. }
        | HarnessEvent::ReadFailed { .. }
        | HarnessEvent::NewClient(_)
        | HarnessEvent::SupervisedWriterCleanupComplete { .. }
        | HarnessEvent::ComponentIngressReady => {
            panic!("reader forwarded before connect command")
        }
    }

    assert!(h.runtime_io.bus.connection(&conn_id).is_some());
    assert!(h.extensions.entries.contains_key(&conn_id));
    assert!(
        h.session_runtime
            .lifecycle_messages
            .iter()
            .any(|m| m == "extension late-tool starting")
    );

    let event = h.expand_component_ingress_wake(
        h.runtime_io
            .rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader should forward after connect ack"),
    );
    match event {
        HarnessEvent::FromConnection {
            connection_id,
            message,
            ..
        } => {
            assert_eq!(connection_id, conn_id);
            assert!(matches!(message.as_ref(), HarnessInputMessage::Hello(_)));
        }
        HarnessEvent::Command(_)
        | HarnessEvent::Disconnected { .. }
        | HarnessEvent::ReadFailed { .. }
        | HarnessEvent::NewClient(_)
        | HarnessEvent::SupervisedWriterCleanupComplete { .. }
        | HarnessEvent::ComponentIngressReady => {
            panic!("unexpected harness event after connect ack")
        }
    }

    h.shutdown().expect("shutdown");
}

#[test]
fn role_disabled_tool_is_reported_without_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                engineer: {
                    roles: {
                        engineer: { disable_tools: ["shell"] },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write harness");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir),
        state_dir: Some(state_dir.clone()),
    };
    let mut h = echo_harness_with_dirs("s1", state_dir, dirs).expect("start");

    h.config.selected_model = Some("test/model".into());
    h.config.selected_role = "engineer".to_owned();
    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-x"), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "do it".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-x"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
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
    .expect("disabled tool call should be handled");

    let expected = prompt_snapshot_tool_error_message(&ToolName::new("shell"));
    assert!(default_agent_tree(&h).nodes().iter().any(|node| {
        matches!(
            &node.entry,
            AgentEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message }
                                if message == &expected
                        )
                })
        )
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn unavailable_tool_name_does_not_panic_and_surfaces_error() {
    // Valid Tau-visible tool names that cannot be routed are model
    // errors, not malformed transcript structure. Commit the assistant
    // call and add a terminal tool error so the next prompt contains a
    // matched function_call/function_call_output pair.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Pre-seed as if the agent had just been prompted and is now
    // responding with tool_calls.
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-x"), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "do it".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    let response = ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-x"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: ToolName::new("not_a_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
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
    };

    h.handle_provider_response_finished(response)
        .expect("invalid tool call must not panic");

    // The call must be gone from both the pending queue and the
    // in-flight set — rejection fully completes it.
    assert!(h.tool_routing.tool_runtime.tool_turn.is_empty());

    // The error should have been persisted on s1's history so the
    // agent sees it on the next turn — as a Requested + Error pair
    // under the same call_id, so the Responses-API serializer can
    // emit a matching `function_call` / `function_call_output`
    // without the latter looking unpaired.
    let expected = unavailable_tool_error_message(&ToolName::new("not_a_tool"));
    let mut saw_call = false;
    let mut saw_error = false;
    for node in default_agent_tree(&h).nodes() {
        match &node.entry {
            AgentEntry::AssistantResponse { output_items, .. } => {
                saw_call |= output_items.iter().any(|item| {
                    matches!(item, ContextItem::ToolCall(call) if call.call_id.as_str() == "c1")
                });
            }
            AgentEntry::ToolResults { items } => {
                saw_error |= items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message }
                                if message == &expected
                        )
                });
            }
            _ => {}
        }
    }
    assert!(
        saw_call && saw_error,
        "rejected call should leave both the assistant tool call and an error result \
         matching tool_use / tool_result pair"
    );

    h.shutdown().expect("shutdown");
}

/// Ensures empty provider call ids become synthetic tool errors instead of an
/// event-loop error that leaves prompt bookkeeping wedged.
#[test]
fn empty_tool_call_id_becomes_model_visible_tool_error() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-x"), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "do it".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-x"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "".into(),
                name: ToolName::new("agent_start"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "".into(),
                name: ToolName::new("agent_start"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("empty call ids should be terminalized as tool errors");

    assert!(h.tool_routing.tool_runtime.tool_turn.is_empty());
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tools
            .contains_key(&ToolCallId::from(""))
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&ToolCallId::from(""))
    );

    let mut assistant_call_ids = Vec::new();
    let mut tool_error_ids = Vec::new();
    for node in default_agent_tree(&h).nodes() {
        match &node.entry {
            AgentEntry::AssistantResponse { output_items, .. } => {
                assistant_call_ids.extend(output_items.iter().filter_map(|item| match item {
                    ContextItem::ToolCall(call) => Some(call.call_id.to_string()),
                    _ => None,
                }));
            }
            AgentEntry::ToolResults { items } => {
                tool_error_ids.extend(items.iter().filter_map(|item| match &item.status {
                    ToolResultStatus::Error { message } if message.contains("empty call_id") => {
                        Some(item.call_id.to_string())
                    }
                    _ => None,
                }));
            }
            _ => {}
        }
    }
    assert_eq!(
        assistant_call_ids,
        vec!["invalid_tool_call_sp-x_1", "invalid_tool_call_sp-x_2"]
    );
    assert_eq!(tool_error_ids, assistant_call_ids);

    h.shutdown().expect("shutdown");
}

/// Ensures duplicate provider call ids are normalized before they reach maps
/// keyed by call id, while the duplicate is reported back to the model.
#[test]
fn duplicate_tool_call_id_becomes_model_visible_tool_error() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-x"), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-x"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "dup".into(),
                name: ToolName::new("not_a_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "dup".into(),
                name: ToolName::new("not_a_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("duplicate call ids should not wedge the harness");

    assert!(h.tool_routing.tool_runtime.tool_turn.is_empty());
    let mut assistant_call_ids = Vec::new();
    let mut duplicate_error_ids = Vec::new();
    for node in default_agent_tree(&h).nodes() {
        match &node.entry {
            AgentEntry::AssistantResponse { output_items, .. } => {
                assistant_call_ids.extend(output_items.iter().filter_map(|item| match item {
                    ContextItem::ToolCall(call) => Some(call.call_id.to_string()),
                    _ => None,
                }));
            }
            AgentEntry::ToolResults { items } => {
                duplicate_error_ids.extend(items.iter().filter_map(|item| match &item.status {
                    ToolResultStatus::Error { message }
                        if message.contains("duplicate tool call_id") =>
                    {
                        Some(item.call_id.to_string())
                    }
                    _ => None,
                }));
            }
            _ => {}
        }
    }
    assert_eq!(assistant_call_ids, vec!["dup", "invalid_tool_call_sp-x_2"]);
    assert_eq!(duplicate_error_ids, vec!["invalid_tool_call_sp-x_2"]);

    h.shutdown().expect("shutdown");
}

/// Ensures a provider cannot reuse a call id from an earlier completed turn.
#[test]
fn reused_prior_tool_call_id_becomes_model_visible_tool_error() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-y");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-y"), cid.clone());
    h.tool_routing
        .tool_runtime
        .completed_tool_calls
        .insert("old-call".into());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-y"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "old-call".into(),
            name: ToolName::new("not_a_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("reused prior call id should not wedge the harness");

    assert!(h.tool_routing.tool_runtime.tool_turn.is_empty());
    let mut assistant_call_ids = Vec::new();
    let mut reused_error_ids = Vec::new();
    for node in default_agent_tree(&h).nodes() {
        match &node.entry {
            AgentEntry::AssistantResponse { output_items, .. } => {
                assistant_call_ids.extend(output_items.iter().filter_map(|item| match item {
                    ContextItem::ToolCall(call) => Some(call.call_id.to_string()),
                    _ => None,
                }));
            }
            AgentEntry::ToolResults { items } => {
                reused_error_ids.extend(items.iter().filter_map(|item| match &item.status {
                    ToolResultStatus::Error { message }
                        if message.contains("reused prior tool call_id") =>
                    {
                        Some(item.call_id.to_string())
                    }
                    _ => None,
                }));
            }
            _ => {}
        }
    }
    assert_eq!(assistant_call_ids, vec!["invalid_tool_call_sp-y_1"]);
    assert_eq!(reused_error_ids, assistant_call_ids);

    h.shutdown().expect("shutdown");
}

#[test]
fn cancel_after_agent_thinking_terminalizes_tool_calls_before_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-x"), cid.clone());
    let target_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_client_event(
        "ui",
        TestProtocolItem::Event(Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(target_agent_id.clone()),
            agent_prompt_id: None,
        })),
    )
    .expect("cancel");

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-x"),
        agent_id: target_agent_id,
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c1".into(),
                name: ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Null,
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c2".into(),
                name: ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Null,
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("response");

    assert!(h.tool_routing.tool_runtime.tool_turn.is_empty());
    assert!(matches!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("conversation")
            .turn
            .turn_state,
        AgentTurnState::Idle
    ));
    let cancelled: Vec<_> = default_agent_tree(&h)
        .nodes()
        .iter()
        .filter_map(|node| match &node.entry {
            AgentEntry::ToolResults { items } => Some(items.iter()),
            _ => None,
        })
        .flatten()
        .filter(|item| matches!(item.status, ToolResultStatus::Cancelled { .. }))
        .map(|item| item.call_id.as_str().to_owned())
        .collect();
    assert_eq!(cancelled, vec!["c1".to_owned(), "c2".to_owned()]);
}

#[test]
fn cancel_during_tools_terminalizes_inflight_calls() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _tool_events = connect_test_tool(&mut h, "conn-cancel-tools");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-cancel-tools"),
        staged_tool_spec("slow_a"),
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-cancel-tools"),
        staged_tool_spec("slow_b"),
    );

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-x"), cid.clone());
    let target_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-x"),
        agent_id: target_agent_id.clone(),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c1".into(),
                name: ToolName::new("slow_a"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c2".into(),
                name: ToolName::new("slow_b"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("response");
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_in_flight(&ToolCallId::from("c1"))
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_in_flight(&ToolCallId::from("c2"))
    );
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);

    h.handle_client_event(
        "ui",
        TestProtocolItem::Event(Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(target_agent_id),
            agent_prompt_id: None,
        })),
    )
    .expect("cancel");

    assert!(h.tool_routing.tool_runtime.tool_turn.is_empty());
    assert!(matches!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("conversation")
            .turn
            .turn_state,
        AgentTurnState::Idle
    ));
    let cancelled: Vec<_> = default_agent_tree(&h)
        .nodes()
        .iter()
        .filter_map(|node| match &node.entry {
            AgentEntry::ToolResults { items } => Some(items.iter()),
            _ => None,
        })
        .flatten()
        .filter(|item| matches!(item.status, ToolResultStatus::Cancelled { .. }))
        .map(|item| item.call_id.as_str().to_owned())
        .collect();
    assert_eq!(cancelled, vec!["c1".to_owned(), "c2".to_owned()]);
}

#[test]
fn provider_disconnect_terminates_event_loop() {
    // Providers are the only prompt executors now. If the selected provider
    // disconnects, keeping the harness alive would leave any in-flight turn
    // without an execution client and can wedge the UI. Treat provider exit as
    // fatal instead of respawning it like a tool extension.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let provider_id = h
        .extension_connection_id("provider")
        .expect("provider")
        .to_owned();

    h.runtime_io
        .tx
        .send(HarnessEvent::Disconnected {
            connection_id: crate::test_connection_id(provider_id),
        })
        .expect("queue provider disconnect");

    let err = h
        .run_event_loop(None, false)
        .expect_err("provider disconnect should terminate harness");
    assert!(matches!(
        err,
        HarnessError::Participant(message) if message == "provider disconnected"
    ));

    h.shutdown().expect("shutdown");
}

/// SIGINT/SIGTERM forwarding must wake an otherwise idle foreground daemon and
/// return through its coordinated shutdown path rather than terminating it
/// asynchronously inside a signal handler.
#[test]
fn termination_command_wakes_idle_event_loop() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.runtime_io
        .tx
        .send(HarnessEvent::Command(HarnessCommand::Shutdown(
            ShutdownCause::ExternalSignal,
        )))
        .expect("queue termination command");

    h.run_event_loop(None, false)
        .expect("termination command exits cleanly");
    h.shutdown().expect("shutdown");
}

/// A crashing tool receives three delayed replacements for the whole session,
/// and the final crash emits one mandatory disable notice.
#[test]
fn crashing_supervised_tool_uses_fake_clock_and_stops_after_three_restarts() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let extension_name = "x".repeat(tau_proto::EXTENSION_NAME_MAX_BYTES);
    connect_supervised_test_process(
        &mut h,
        supervised_test_config(&extension_name, "exit 1"),
        tau_proto::ClientKind::Tool,
    );
    let mut now = Instant::now();

    for expected_attempt in 0..=MAX_EXTENSION_RESTART_ATTEMPTS {
        let connection_id = drive_crashed_extension_cleanup(&mut h, &extension_name, now);
        let entry = &h.extensions.entries[&connection_id];
        assert_eq!(entry.restart_attempt, expected_attempt);
        if expected_attempt == MAX_EXTENSION_RESTART_ATTEMPTS {
            assert!(!entry.respawn_allowed);
            assert!(!h.extensions.restart_deadlines.contains_key(&connection_id));
            break;
        }

        let restart_at = h.extensions.restart_deadlines[&connection_id];
        assert_eq!(restart_at, now + EXTENSION_RESTART_DELAY);
        h.process_runtime_deadlines_at(restart_at - Duration::from_nanos(1));
        assert_eq!(
            h.extensions.entries[&connection_id].restart_attempt,
            expected_attempt
        );
        h.process_runtime_deadlines_at(restart_at);
        now = restart_at;
    }

    let restart_events = event_log_events(&h)
        .iter()
        .filter(|event| matches!(event, Event::ExtensionRestarting(_)))
        .count();
    assert_eq!(restart_events, MAX_EXTENSION_RESTART_ATTEMPTS as usize);
    let disable_notices = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::HarnessNotice(notice)
                if notice.message.contains("automatic restart attempts") =>
            {
                Some(notice)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(disable_notices.len(), 1);
    assert_eq!(disable_notices[0].purpose, tau_proto::NoticePurpose::Alert);
    assert_eq!(disable_notices[0].level, tau_proto::NoticeLevel::Warning);
    assert!(disable_notices[0].message.len() <= MAX_EXTENSION_RESTART_NOTICE_BYTES);
    h.shutdown().expect("shutdown");
}

#[test]
fn duplicate_tool_result_is_discarded() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let mut h = echo_harness(&sp).expect("start");

    // Fabricate a tool result for a call_id with no pending runtime metadata.
    let result = h.handle_extension_event(
        "fake-ext",
        TestProtocolItem::Event(Event::ToolResultReported(ToolResult {
            presentation: Default::default(),
            call_id: "orphan-call".into(),
            tool_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: tau_proto::CborValue::Text("stale data".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        })),
    );
    // Should not error — just emits a warning and discards.
    assert!(result.is_ok());
}

/// Extension handshake stores declared bridge authority on the configured
/// connection entry used by event admission.
#[test]
fn extension_hello_installs_declared_peer_capabilities() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    connect_handshaking_tool(&mut h, "bridge");
    h.extensions
        .entries
        .get_mut("bridge")
        .expect("extension")
        .state = ExtensionState::Spawning;
    h.handle_extension_message(
        &crate::test_connection_id("bridge"),
        TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("bridge"),
            client_kind: tau_proto::ClientKind::Tool,
            expected_session_id: None,
            capabilities: vec![tau_proto::PeerCapability::MessageBridge],
        }),
    )
    .expect("hello");
    assert!(
        h.extensions.entries["bridge"]
            .peer_capabilities
            .contains(&tau_proto::PeerCapability::MessageBridge)
    );
}

/// Ensures an explicit socket-client disconnect goes through the same cleanup
/// path as an async disconnect, removing both bus and client-writer state.
#[test]
fn explicit_socket_disconnect_cleans_client_writer_and_bus_state() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let (server_end, _client_end) = UnixStream::pair().expect("pair");
    h.accept_client(server_end).expect("accept client");
    let socket_conn = h
        .runtime_io
        .bus
        .connections()
        .into_iter()
        .find(|metadata| metadata.origin == ConnectionOrigin::Socket)
        .map(|metadata| metadata.id)
        .expect("socket client connection");
    assert!(h.runtime_io.bus.connection(&socket_conn).is_some());
    assert!(h.ui_runtime.client_writers.contains_key(&socket_conn));

    h.runtime_io
        .tx
        .send(HarnessEvent::from_connection_for_test(
            socket_conn.clone(),
            HarnessInputMessage::Disconnect(Disconnect {
                reason: Some("test explicit disconnect".to_owned()),
            }),
        ))
        .expect("queue explicit disconnect");

    h.run_event_loop(Some(1), false).expect("event loop exits");

    assert!(h.runtime_io.bus.connection(&socket_conn).is_none());
    assert!(!h.ui_runtime.client_writers.contains_key(&socket_conn));
}

/// A rejected startup handshake must finish its queued disconnect write before
/// teardown removes the writer and allows the harness process to exit.
#[test]
fn rejected_startup_handshake_flushes_disconnect_before_teardown() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let committed = Arc::new(Mutex::new(Vec::new()));
    let completed_flushes = Arc::new(AtomicUsize::new(0));
    let client_id = h
        .accept_client_io(
            path_std_io::empty(),
            RecordingWriter {
                committed: Arc::clone(&committed),
                flushes: 0,
                completed_flushes: Arc::clone(&completed_flushes),
            },
            None,
            ConnectionOrigin::Socket,
            ClientWriterFailure::Report,
        )
        .expect("accept startup client");
    let requested = tau_proto::SessionId::parse("different-session").expect("valid session id");

    let error = h
        .handle_startup_from_connection(
            &client_id,
            HarnessInputMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: crate::test_extension_name("attach-ui"),
                client_kind: tau_proto::ClientKind::Ui,
                expected_session_id: Some(requested.clone()),
                capabilities: Default::default(),
            }),
        )
        .expect_err("session mismatch terminates the startup handshake");

    assert!(
        error
            .to_string()
            .contains("disconnected during startup handshake")
    );
    assert!(h.runtime_io.bus.connection(&client_id).is_none());
    assert!(!h.ui_runtime.client_writers.contains_key(&client_id));
    assert_eq!(
        completed_flushes.load(Ordering::Acquire),
        1,
        "the cursor barrier observes the frame's mandatory delivery flush"
    );
    let output = committed.lock().expect("committed output").clone();
    let mut reader = HarnessOutputReader::new(BufReader::new(output.as_slice()));
    let message = reader
        .read_message()
        .expect("read handshake rejection")
        .expect("handshake rejection");
    let HarnessOutputMessage::Disconnect(disconnect) = message else {
        panic!("expected disconnect frame");
    };
    let reason = disconnect.reason.expect("disconnect reason");
    assert!(reason.contains(requested.as_str()));
    assert!(reason.contains(h.session_runtime.current_session_id.as_str()));
}

/// A runtime handshake rejection must drain its terminal response before
/// teardown while still accounting for the served socket client.
#[test]
fn rejected_runtime_handshake_flushes_disconnect_before_teardown() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let committed = Arc::new(Mutex::new(Vec::new()));
    let completed_flushes = Arc::new(AtomicUsize::new(0));
    let client_id = h
        .accept_client_io(
            path_std_io::empty(),
            RecordingWriter {
                committed: Arc::clone(&committed),
                flushes: 0,
                completed_flushes: Arc::clone(&completed_flushes),
            },
            None,
            ConnectionOrigin::Socket,
            ClientWriterFailure::Report,
        )
        .expect("accept runtime client");
    let requested = tau_proto::SessionId::parse("different-session").expect("valid session id");
    let message = HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: crate::test_extension_name("attach-ui"),
        client_kind: tau_proto::ClientKind::Ui,
        expected_session_id: Some(requested.clone()),
        capabilities: Default::default(),
    });
    let frame_bytes = lifecycle_input_frame_bytes(&message);
    let mut served_clients = 0;
    let mut exit_on_disconnect = true;

    h.handle_runtime_connection_message(
        client_id.clone(),
        Box::new(message),
        frame_bytes,
        &mut served_clients,
        &mut exit_on_disconnect,
    )
    .expect("handle runtime rejection");

    assert_eq!(served_clients, 1);
    assert!(exit_on_disconnect);
    assert!(h.runtime_io.bus.connection(&client_id).is_none());
    assert!(!h.ui_runtime.client_writers.contains_key(&client_id));
    assert_eq!(
        completed_flushes.load(Ordering::Acquire),
        1,
        "the cursor barrier observes the frame's mandatory delivery flush"
    );
    let output = committed.lock().expect("committed output").clone();
    let mut reader = HarnessOutputReader::new(BufReader::new(output.as_slice()));
    let message = reader
        .read_message()
        .expect("read runtime rejection")
        .expect("runtime rejection");
    let HarnessOutputMessage::Disconnect(disconnect) = message else {
        panic!("expected disconnect frame");
    };
    let reason = disconnect.reason.expect("disconnect reason");
    assert!(reason.contains(requested.as_str()));
    assert!(reason.contains(h.session_runtime.current_session_id.as_str()));
}

/// A client-requested runtime disconnect must not wait behind stalled outbound
/// data because the client did not request a terminal response.
#[test]
fn client_requested_disconnect_does_not_drain_stalled_writer() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let (started_tx, started_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::channel();
    let client_id = h
        .accept_client_io(
            path_std_io::empty(),
            StalledWriter {
                started: Some(started_tx),
                release: release_rx,
            },
            None,
            ConnectionOrigin::Socket,
            ClientWriterFailure::Report,
        )
        .expect("accept runtime client");
    h.runtime_io
        .bus
        .send_to(
            &client_id,
            None,
            HarnessOutputMessage::Disconnect(Disconnect {
                reason: Some("queued before client close".to_owned()),
            }),
        )
        .expect("queue output");
    started_rx.recv().expect("writer reached stalled output");

    let message = HarnessInputMessage::Disconnect(Disconnect { reason: None });
    let frame_bytes = lifecycle_input_frame_bytes(&message);
    let (handled_tx, handled_rx) = mpsc::channel();
    let release_guard = std::thread::spawn(move || {
        let handled_before_release = handled_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        release_tx.send(()).expect("release stalled writer");
        handled_before_release
    });
    let mut served_clients = 0;
    let mut exit_on_disconnect = true;

    h.handle_runtime_connection_message(
        client_id,
        Box::new(message),
        frame_bytes,
        &mut served_clients,
        &mut exit_on_disconnect,
    )
    .expect("handle client-requested disconnect");
    let _ = handled_tx.send(());
    assert!(
        release_guard.join().expect("release guard"),
        "client-requested disconnect waited for stalled output"
    );
    assert_eq!(served_clients, 1);
    assert!(exit_on_disconnect);
}

/// A protocol mismatch is client-local and must not depend on unrelated
/// configured-extension startup or disconnect another client.
#[test]
fn client_hello_protocol_mismatch_disconnects_only_client() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(sp.join("config")),
        state_dir: Some(sp.join("runtime")),
    };
    let mut h = Harness::from_config_without_startup_environment(
        &Config::default(),
        &sp,
        dirs,
        "s1",
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::Durable,
    )
    .expect("start extensionless harness");
    let observer = connect_test_client(&mut h, "observer", tau_proto::ClientKind::Ui);
    let events = connect_test_client(&mut h, "stale-ui", tau_proto::ClientKind::Ui);

    let keep = h
        .handle_client_event(
            "stale-ui",
            TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION + 1,
                client_name: crate::test_extension_name("stale-ui"),
                client_kind: tau_proto::ClientKind::Ui,
                expected_session_id: None,
                capabilities: Default::default(),
            })),
        )
        .expect("mismatched ui hello should not fail harness");

    assert!(!keep);
    let events = events.lock().expect("events");
    assert!(
        events.iter().any(|event| matches!(
            &event.frame,
            HarnessOutputMessage::Disconnect(disconnect)
                if disconnect
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("unsupported protocol version from stale-ui"))
        )),
        "expected disconnect for stale UI, got: {events:?}"
    );
    assert!(
        observer.lock().expect("observer events").is_empty(),
        "a client-local rejection must not disconnect another client"
    );
}

/// A peer request with an in-flight agent call ID must commit before rejection,
/// while preserving the original call's owner, metadata, and terminal path.
#[test]
fn extension_tool_request_cannot_reuse_in_flight_agent_call_id() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    connect_ready_configured_extension(
        &mut h,
        "owner-ext",
        "configured-owner",
        tau_proto::ClientKind::Tool,
    );
    connect_ready_configured_extension(
        &mut h,
        "hijacker-ext",
        "configured-hijacker",
        tau_proto::ClientKind::Provider,
    );
    let cid = ensure_test_user_agent(&mut h);
    let owner_agent_id = durable_agent_id_for_conversation(&h, &cid);
    let call_id: ToolCallId = "shared-call".into();
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: test_agent_prompt_id("sp-open"),
            agent_id: owner_agent_id.clone(),
            output_items: vec![ContextItem::ToolCall(ToolCallItem {
                call_id: call_id.clone(),
                name: ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("read"),
            internal_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .pending_tool_providers
        .insert(call_id.clone(), crate::test_connection_id("owner-ext"));
    let completed_before = h.tool_routing.tool_runtime.completed_tool_calls.clone();
    let (in_flight_before, total_before) = {
        let agent = &h.agent_runtime.agent_registry.agents[&cid];
        (agent.execution.tools_in_flight, agent.execution.tools_total)
    };

    h.handle_extension_event(
        "hijacker-ext",
        TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ToolRequest(tau_proto::ToolRequest {
                call_id: call_id.clone(),
                tool_name: ToolName::new("write"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                agent_id: crate::parse_agent_id("agent-1"),
                originator: tau_proto::PromptOriginator::User,
            })),
            persist: false,
        })),
    )
    .expect("reject reused extension call id");

    assert_eq!(
        h.tool_routing.tool_runtime.tool_agents.get(&call_id),
        Some(&cid)
    );
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tools
            .get(&call_id)
            .map(|tool| tool.name.as_str()),
        Some("read")
    );
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .get(&call_id)
            .map(tau_proto::ConnectionId::as_str),
        Some("owner-ext")
    );
    assert_eq!(
        h.tool_routing.tool_runtime.completed_tool_calls,
        completed_before
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .tools_in_flight,
        in_flight_before
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .tools_total,
        total_before
    );
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::ToolRejected(rejected) if rejected.call_id == call_id
        )
    }));
    assert!(event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::HarnessNotice(info) if info.message.contains("already-known call_id")
        )
    }));
    let events = event_log_events(&h);
    let request_pos = events
        .iter()
        .position(
            |event| matches!(event, Event::ToolRequest(request) if request.call_id == call_id),
        )
        .expect("duplicate request commits");
    let notice_pos = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::HarnessNotice(info) if info.message.contains("already-known call_id")
            )
        })
        .expect("duplicate notice");
    assert!(request_pos < notice_pos);
    assert!(
        !default_agent_tree(&h)
            .nodes()
            .iter()
            .any(|node| matches!(node.entry, AgentEntry::ToolResults { .. }))
    );

    h.handle_extension_event(
        "owner-ext",
        TestProtocolItem::Event(Event::ToolResultReported(ToolResult {
            presentation: Default::default(),
            call_id: call_id.clone(),
            tool_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::default(),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("original owner can still complete");
    let tool_results: Vec<_> = default_agent_tree(&h)
        .nodes()
        .iter()
        .filter_map(|node| match &node.entry {
            AgentEntry::ToolResults { items } => Some(items),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0][0].call_id, call_id);
    assert!(matches!(
        tool_results[0][0].status,
        ToolResultStatus::Success
    ));

    h.shutdown().expect("shutdown");
}

#[test]
fn resumed_historical_tool_call_id_reuse_becomes_model_visible_tool_error() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    {
        let mut h = echo_harness(&sp).expect("start");
        let cid = ensure_test_user_agent(&mut h);
        seed_agent_thinking(&mut h, &cid, "sp-old");
        h.prompt_coordination
            .prompt_runtime
            .agents
            .insert(test_agent_prompt_id("sp-old"), cid.clone());
        h.handle_provider_response_finished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: test_agent_prompt_id("sp-old"),
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            output_items: vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "historical-call".into(),
                name: ToolName::new("not_a_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        })
        .expect("seed historical call id");
        h.shutdown().expect("shutdown");
    }

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    let cid = test_user_agent(&h);
    seed_agent_thinking(&mut h, &cid, "sp-new");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-new"), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-new"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "historical-call".into(),
            name: ToolName::new("not_a_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("historical reuse should be repaired");

    let mut assistant_call_ids = Vec::new();
    let mut reused_error_ids = Vec::new();
    for node in default_agent_tree(&h).nodes() {
        match &node.entry {
            AgentEntry::AssistantResponse { output_items, .. } => {
                assistant_call_ids.extend(output_items.iter().filter_map(|item| match item {
                    ContextItem::ToolCall(call) => Some(call.call_id.to_string()),
                    _ => None,
                }));
            }
            AgentEntry::ToolResults { items } => {
                reused_error_ids.extend(items.iter().filter_map(|item| match &item.status {
                    ToolResultStatus::Error { message }
                        if message.contains("reused prior tool call_id") =>
                    {
                        Some(item.call_id.to_string())
                    }
                    _ => None,
                }));
            }
            _ => {}
        }
    }
    assert!(assistant_call_ids.iter().any(|id| id == "historical-call"));
    assert!(
        assistant_call_ids
            .iter()
            .any(|id| id == "invalid_tool_call_sp-new_1")
    );
    assert_eq!(reused_error_ids, vec!["invalid_tool_call_sp-new_1"]);

    h.shutdown().expect("shutdown");
}

#[test]
fn disconnect_unregisters_tools_before_advancing_queued_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    connect_test_tool(&mut h, "drop-ext");
    h.tool_routing.registry.register(
        &crate::test_connection_id("drop-ext"),
        ToolSpec {
            name: ToolName::new("stale_tool"),
            model_visible_name: None,
            description: Some("stale".to_owned()),
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let cid = ensure_test_user_agent(&mut h);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .pending_prompts
        .push_back(PendingPrompt::user("run".to_owned()));

    h.handle_disconnect(&crate::test_connection_id("drop-ext"));

    let prompts: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentPromptCreated(prompt) => Some(prompt),
            _ => None,
        })
        .collect();
    let prompt = prompts.last().expect("queued prompt dispatched");
    assert!(!prompt_has_tool(prompt, "stale_tool"));

    h.shutdown().expect("shutdown");
}

#[test]
fn disconnect_session_init_completion_waits_until_tool_cleanup() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    connect_test_tool(&mut h, "init-ext");
    h.tool_routing.registry.register(
        &crate::test_connection_id("init-ext"),
        ToolSpec {
            name: ToolName::new("init_stale_tool"),
            model_visible_name: None,
            description: Some("stale".to_owned()),
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    h.session_runtime.turn_state = TurnState::InitializingSession {
        session_id: h.session_runtime.current_session_id.clone(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: HashSet::from([tau_proto::ConnectionId::parse("init-ext")
            .expect("test connection id must satisfy the identifier grammar")]),
    };
    let cid = ensure_test_user_agent(&mut h);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .pending_prompts
        .push_back(PendingPrompt::user("run".to_owned()));

    h.handle_disconnect(&crate::test_connection_id("init-ext"));

    assert!(h.session_runtime.turn_state.is_idle());
    let prompts: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentPromptCreated(prompt) => Some(prompt),
            _ => None,
        })
        .collect();
    let prompt = prompts.last().expect("queued prompt dispatched");
    assert!(!prompt_has_tool(prompt, "init_stale_tool"));

    h.shutdown().expect("shutdown");
}

/// A non-tool extension query that unexpectedly requests a tool receives one
/// terminal error before side-conversation teardown removes its routing state.
#[test]
fn non_tool_extension_query_tool_call_gets_terminal_error_before_teardown() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
    {
        let conv = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        conv.identity.originator = tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("query-ext"),
            query_id: "query-1".into(),
        };
        conv.identity.source_connection = Some(crate::test_connection_id(HARNESS_CONNECTION_ID));
        conv.identity.parent_tool_call_id = None;
    }
    seed_agent_thinking(&mut h, &cid, "sp-query");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-query"), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-query"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "query-call".into(),
            name: ToolName::new("not_a_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("query-ext"),
            query_id: "query-1".into(),
        },
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("non-tool query tool call terminalized");

    let tree = h
        .session_runtime
        .agent_store
        .agent(durable_agent_id.as_str())
        .expect("removed agent tree remains");
    assert!(tree.nodes().iter().any(|node| matches!(
        &node.entry,
        AgentEntry::ToolResults { items }
            if items.iter().any(|item| item.call_id == "invalid_tool_call_sp-query_1"
                && matches!(item.status, ToolResultStatus::Error { .. }))
    )));
    let events = event_log_events(&h);
    let tool_error_pos = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::ProviderToolError(error)
                    if error.call_id == "invalid_tool_call_sp-query_1"
            )
        })
        .expect("terminal tool error event");
    let result_pos = events
        .iter()
        .position(|event| matches!(event, Event::StartAgentResult(_)))
        .expect("start agent result event");
    assert!(tool_error_pos < result_pos);

    h.shutdown().expect("shutdown");
}

/// A rejected non-tool side query settles and re-evaluates its pending message
/// wake before the unparented query completes exactly once and is removed.
#[test]
fn non_tool_extension_query_pending_message_still_terminalizes_tool_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
    {
        let conv = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        conv.identity.originator = tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("query-ext"),
            query_id: "query-2".into(),
        };
        conv.identity.source_connection = Some(crate::test_connection_id(HARNESS_CONNECTION_ID));
        conv.identity.parent_tool_call_id = None;
    }
    seed_agent_thinking(&mut h, &cid, "sp-query-pending");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-query-pending"), cid.clone());
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("query-pending-message")
                .expect("test identifier must satisfy its grammar"),
            sender_id: tau_proto::AgentId::parse("manager").expect("sender id"),
            sender_session_id: None,
            recipient_id: durable_agent_id.clone(),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "notice".to_owned(),
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-query-pending"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "query-pending-call".into(),
            name: ToolName::new("not_a_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("query-ext"),
            query_id: "query-2".into(),
        },
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("pending-message branch terminalizes tool call");

    let tree = h
        .session_runtime
        .agent_store
        .agent(durable_agent_id.as_str())
        .expect("agent tree remains");
    assert!(tree.nodes().iter().any(|node| matches!(
        &node.entry,
        AgentEntry::ToolResults { items }
            if items.iter().any(|item| item.call_id == "invalid_tool_call_sp-query-pending_1"
                 && matches!(item.status, ToolResultStatus::Error { .. }))
    )));
    assert!(
        !h.agent_runtime.agent_registry.agents.contains_key(&cid),
        "unparented rejected side query did not complete"
    );
    let events = event_log_events(&h);
    let tool_error_pos = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::ProviderToolError(error)
                    if error.call_id == "invalid_tool_call_sp-query-pending_1"
            )
        })
        .expect("rejected tool terminal");
    let results = events
        .into_iter()
        .enumerate()
        .filter_map(|(position, event)| match event {
            Event::StartAgentResult(result) if result.query_id == "query-2" => {
                Some((position, result))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [(result_pos, result)] = results.as_slice() else {
        panic!("expected one rejected side-conversation result, got {results:?}");
    };
    assert!(tool_error_pos < *result_pos);
    assert!(result.text.is_empty());
    assert_eq!(
        result.error.as_deref(),
        Some("non-tool extension query attempted to call 1 tool(s); refusing to execute")
    );

    h.shutdown().expect("shutdown");
}

/// A call cut off by the output cap must remain inspectable while never
/// executing or activating a synthetic tool-error inference.
#[test]
fn length_stopped_tool_call_is_preserved_but_never_executed() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-length");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-length"), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-length"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "length-call".into(),
            name: ToolName::new("not_a_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::Length,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("length stop tool call terminalized");

    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tools
            .contains_key("length-call")
    );
    assert!(
        !default_agent_tree(&h)
            .nodes()
            .iter()
            .any(|node| matches!(&node.entry, AgentEntry::ToolResults { .. }))
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ProviderResponseFinished(response)
            if response.stop_reason == tau_proto::ProviderStopReason::Length
                && response.output_items.iter().any(|item| matches!(
                    item,
                    ContextItem::ToolCall(call) if call.call_id == "length-call"
                ))
                && response.output_length_disposition
                    == tau_proto::OutputLengthDisposition::None
    )));

    h.shutdown().expect("shutdown");
}

/// A committed tool round must start a new reasoning-only run, allowing one
/// later same-turn successor and preserving that spent state on cold replay.
#[test]
fn output_length_tool_round_rearms_same_turn_and_cold_replay() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    let tool_frames = connect_test_tool(&mut h, "length-rearm-tool");
    h.tool_routing.registry.register(
        &crate::test_connection_id("length-rearm-tool"),
        staged_tool_spec("length_rearm_step"),
    );
    h.submit_user_prompt(test_session_id("s1"), "take two steps".to_owned())
        .expect("submit");

    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 3))
        .expect("first length");
    let first_successor = read_nth_prompt_created(&h, 1);
    let cid = h
        .agent_id_for_prompt(&first_successor.agent_prompt_id)
        .expect("runtime agent");
    let spent_head = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("pre-action selected head");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: first_successor.agent_prompt_id.clone(),
        agent_id: first_successor.agent_id.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "length-rearm-call".into(),
            name: ToolName::new("length_rearm_step"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        provider_attempt: Default::default(),
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("committed tool response");
    assert!(sink_has_tool_invoke(&tool_frames, "length-rearm-call"));
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .get("length-rearm-call")
            .map(tau_proto::ConnectionId::as_str),
        Some("length-rearm-tool")
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .output_length_continuation,
        OutputLengthContinuationState::None
    );
    let rearmed_head = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("action response head");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("runtime agent")
        .identity
        .head = Some(spent_head);
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: source.agent_id.clone(),
            head: tau_proto::AgentHead::Node(spent_head),
        }),
    );
    assert!(matches!(
        &h.agent_runtime.agent_registry.agents[&cid].turn.output_length_continuation,
        OutputLengthContinuationState::Spent { outer_turn_id }
            if *outer_turn_id == tau_proto::AgentOuterTurnId::for_prompt(&source.agent_prompt_id)
    ));
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("runtime agent")
        .identity
        .head = Some(rearmed_head);
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: source.agent_id.clone(),
            head: tau_proto::AgentHead::Node(rearmed_head),
        }),
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .output_length_continuation,
        OutputLengthContinuationState::None
    );

    let Event::ToolResultReported(result) =
        test_tool_result("length-rearm-call", "length_rearm_step")
    else {
        unreachable!("test result helper");
    };
    h.handle_extension_tool_result(&crate::test_connection_id("length-rearm-tool"), result);
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        3,
        "{:#?}",
        event_log_events(&h)
    );
    let later = read_nth_prompt_created(&h, 2);
    h.handle_provider_response_finished(reasoning_only_length_response(&later, 5))
        .expect("later length");
    let second_successor = read_nth_prompt_created(&h, 3);

    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable events");
    let plans = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(response)
                if matches!(
                    response.output_length_disposition,
                    tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
                ) =>
            {
                Some(response)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(plans.len(), 2);
    let plan_fields = plans
        .iter()
        .map(|response| {
            let tau_proto::OutputLengthDisposition::ContinuationPlanned {
                outer_turn_id,
                successor_agent_prompt_id,
                ordinal,
                limit,
            } = &response.output_length_disposition
            else {
                unreachable!("filtered plans");
            };
            (
                outer_turn_id,
                &response.agent_prompt_id,
                successor_agent_prompt_id,
                ordinal,
                limit,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(plan_fields[0].0, plan_fields[1].0);
    assert_ne!(plan_fields[0].1, plan_fields[1].1);
    assert_eq!(plan_fields[0].2, &first_successor.agent_prompt_id);
    assert_eq!(plan_fields[1].2, &second_successor.agent_prompt_id);
    assert_eq!((*plan_fields[0].3, *plan_fields[0].4), (1, 1));
    assert_eq!((*plan_fields[1].3, *plan_fields[1].4), (1, 1));
    let steers = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentPromptSteered(steer)
                if steer.internal_kind
                    == Some(tau_proto::InternalPromptKind::OutputLengthContinuation) =>
            {
                Some(steer)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(steers.len(), 2);
    assert!(steers.iter().all(|steer| {
        steer.text == tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION
            && !steer.text.contains("<tau_internal>")
            && steer.submission_source == tau_proto::PromptSubmissionSource::HarnessInternal
            && steer.message_class == tau_proto::PromptMessageClass::Internal
            && steer.trusted_internal_spans
                == vec![tau_proto::TrustedInternalSpan {
                    start: 0,
                    end: u32::try_from(steer.text.len()).expect("bounded instruction"),
                }]
    }));

    let cold = tau_core::AgentTree::try_from_events(source.agent_id.clone(), &records)
        .expect("cold replay accepts two same-turn plans");
    assert_eq!(
        cold.output_length_budget_spent_outer_turn(),
        Some(plan_fields[1].0.clone())
    );
    h.handle_provider_response_finished(provider_text_response(
        &second_successor.agent_prompt_id,
        second_successor.agent_id,
        "finished after two runs",
    ))
    .expect("second successor answer");
    let finished_records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("finished durable events");
    assert_eq!(
        finished_records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentOuterTurnFinished(_)))
            .count(),
        1
    );
    h.shutdown().expect("shutdown");
}

/// A parked successor ToolCalls response must not execute or synthesize closure
/// before cancellation rewrites its canonical terminal.
#[test]
fn output_length_tool_calls_terminal_race_never_dispatches_calls() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    let tool_frames = connect_test_tool(&mut h, "length-tool-terminal-race");
    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("length-tool-terminal-race"),
        Event::ToolRegister(tau_proto::ToolRegister {
            publisher_extension_id: crate::test_extension_name("length-tool-terminal-race"),
            publisher_instance_id: 42.into(),
            tool: tau_proto::ToolSpec {
                name: ToolName::new("cancel_test_tool"),
                model_visible_name: None,
                description: Some("must remain undispatched".to_owned()),
                tool_type: tau_proto::ToolType::Function,
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "additionalProperties": false
                })),
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
                background_support: Some(tau_proto::BackgroundSupport::Never),
                examples: Vec::new(),
            },
            tool_group: None,
            prompt_fragment: None,
        }),
        Some(false),
    )
    .expect("register dispatchable tool");
    h.submit_user_prompt(test_session_id("s1"), "cancel tool terminal".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 3))
        .expect("source response");
    let successor = read_nth_prompt_created(&h, 1);
    h.handle_extension_event(
        "length-tool-terminal-race",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register terminal interceptor");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: successor.agent_prompt_id.clone(),
        agent_id: successor.agent_id.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "cancelled-successor-call".into(),
            name: ToolName::new("cancel_test_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("park tool-calls terminal");
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tools
            .contains_key("cancelled-successor-call")
    );
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ToolResult(result) if result.call_id == "cancelled-successor-call"
    )));
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ProviderToolError(error) if error.call_id == "cancelled-successor-call"
    )));
    assert!(tool_frames.lock().expect("tool frames").iter().all(|frame| {
        !matches!(
            peel_inner_event(&frame.frame),
            Some(Event::ToolRequest(request)) if request.call_id == "cancelled-successor-call"
        )
    }));
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(source.agent_id.clone()),
            agent_prompt_id: None,
        },
    );
    h.handle_extension_event(
        "length-tool-terminal-race",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit cancellation-owned terminal");
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tools
            .contains_key("cancelled-successor-call")
    );
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ToolResult(result) if result.call_id == "cancelled-successor-call"
    )));
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ProviderToolError(error) if error.call_id == "cancelled-successor-call"
    )));
    assert!(tool_frames.lock().expect("tool frames").iter().all(|frame| {
        !matches!(
            peel_inner_event(&frame.frame),
            Some(Event::ToolRequest(request)) if request.call_id == "cancelled-successor-call"
        )
    }));
    assert!(!default_agent_tree(&h).nodes().iter().any(|node| matches!(
        &node.entry,
        AgentEntry::ToolResults { items }
            if items.iter().any(|item| item.call_id == "cancelled-successor-call")
    )));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2
    );
    let successor_terminals = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable events")
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == successor.agent_prompt_id
                        && response.output_items.is_empty()
                        && matches!(
                            response.output_length_disposition,
                            tau_proto::OutputLengthDisposition::ContinuationTerminal {
                                outcome: tau_proto::OutputLengthContinuationOutcome::Cancelled,
                                ..
                            }
                        )
            )
        })
        .count();
    assert_eq!(successor_terminals, 1);
    h.shutdown().expect("shutdown");
}

#[test]
fn disconnect_removes_extension_prompt_and_agent_context() {
    let tmp = TempDir::new().expect("temp dir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_test_tool(&mut h, "ctx-ext");
    let contributor = tau_proto::ConnectionId::parse("ctx-ext")
        .expect("test connection id must satisfy the identifier grammar");
    let agent_id = crate::parse_agent_id("agent-1");

    h.apply_extension_prompt_fragment(
        &crate::test_connection_id("ctx-ext"),
        tau_proto::ExtPromptFragmentPublish {
            fragment: tau_proto::PromptFragment::new(
                "ctx-fragment",
                tau_proto::PromptPriority::new(100),
                "stale fragment",
            ),
        },
    );
    h.apply_agent_context_publish(
        &crate::test_connection_id("ctx-ext"),
        tau_proto::ExtAgentContextPublish {
            session_id: test_session_id("test-session"),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            agent_id: agent_id.clone(),
            key: tau_proto::AgentContextKey::from("skills"),
            value: tau_proto::AgentContextValue(serde_json::json!(["stale"])),
        },
    );
    h.prompt_coordination
        .context_discovery
        .agent_context_providers
        .insert(contributor.clone());
    set_test_agent_context_wait(
        &mut h,
        agent_id.clone(),
        HashSet::from([contributor.clone()]),
    );

    h.handle_disconnect(&crate::test_connection_id("ctx-ext"));

    assert!(
        !h.prompt_coordination
            .context_discovery
            .prompt_fragments
            .contains_key(&contributor)
    );
    assert_eq!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(&agent_id)),
        serde_json::json!({})
    );
    assert!(
        !h.prompt_coordination
            .context_discovery
            .agent_context_providers
            .contains(&contributor)
    );
    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent_id)
    );
}

#[test]
fn switch_session_clears_session_scoped_extension_context() {
    let tmp = TempDir::new().expect("temp dir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_test_tool(&mut h, "ctx-ext");
    let contributor = tau_proto::ConnectionId::parse("ctx-ext")
        .expect("test connection id must satisfy the identifier grammar");
    let agent_id = crate::parse_agent_id("agent-1");

    h.apply_extension_prompt_fragment(
        &crate::test_connection_id("ctx-ext"),
        tau_proto::ExtPromptFragmentPublish {
            fragment: tau_proto::PromptFragment::new(
                "ctx-fragment",
                tau_proto::PromptPriority::new(100),
                "old session fragment",
            ),
        },
    );
    h.apply_agent_context_publish(
        &crate::test_connection_id("ctx-ext"),
        tau_proto::ExtAgentContextPublish {
            session_id: test_session_id("test-session"),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            agent_id: agent_id.clone(),
            key: tau_proto::AgentContextKey::from("skills"),
            value: tau_proto::AgentContextValue(serde_json::json!(["old session"])),
        },
    );
    h.prompt_coordination
        .context_discovery
        .agent_context_providers
        .insert(contributor.clone());
    set_test_agent_context_wait(
        &mut h,
        agent_id.clone(),
        HashSet::from([contributor.clone()]),
    );

    h.switch_session(test_session_id("s2"), tau_proto::SessionStartReason::New)
        .expect("switch session");

    assert!(
        h.prompt_coordination
            .context_discovery
            .prompt_fragments
            .contains_key(&contributor)
    );
    let (fragments, tool_fragments) =
        h.gather_sourced_prompt_fragment_groups(&h.config.selected_role);
    assert!(tool_fragments.is_empty());
    assert!(fragments.iter().any(|sourced| {
        sourced.fragment.name == "ctx-fragment"
            && matches!(
                sourced.source,
                PromptFragmentSource::Extension { ref connection_id }
                    if connection_id == &contributor
            )
    }));
    assert_eq!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(&agent_id)),
        serde_json::json!({})
    );
    assert!(
        h.prompt_coordination
            .context_discovery
            .pending_agents
            .is_empty()
    );
    assert!(
        h.prompt_coordination
            .context_discovery
            .agent_context_providers
            .contains(&contributor)
    );
}
/// Prevents steady-state Provider captures from being rejected after activation
/// consumes the transient `ready_received` marker and leaves the peer `Ready`.
#[test]
fn ready_provider_capture_is_legal_operational_traffic() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let provider = harness
        .extensions
        .entries
        .iter()
        .find_map(|(connection_id, entry)| {
            (entry.kind == tau_proto::ClientKind::Provider && entry.state == ExtensionState::Ready)
                .then(|| connection_id.clone())
        })
        .expect("ready provider");
    assert!(
        !harness.extensions.ready_received.contains(&provider),
        "completed activation consumes the transient Ready marker"
    );

    harness
        .handle_extension_message(
            &provider,
            HarnessInputMessage::ProviderDebugCapture(tau_proto::ProviderDebugCapture {
                session_id: test_session_id("missing-session"),
                agent_prompt_id: test_agent_prompt_id("ready-capture"),
                class: tau_proto::ProviderDebugCaptureClass::WebsocketRequest,
                zstd: vec![1, 2, 3],
            }),
        )
        .expect("Ready Provider capture must pass protocol phase validation");

    assert_eq!(
        harness
            .extensions
            .entries
            .get(&provider)
            .expect("provider remains registered")
            .state,
        ExtensionState::Ready
    );
}

#[test]
fn configure_includes_extension_state_dir_and_creates_it() {
    // The configure handshake is the only place an extension learns its
    // persistent state location. Keep the path stable at state/ext/<name> and
    // ensure it exists by the time the extension receives it.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let sink = connect_handshaking_tool(&mut h, "std-email");
    h.extensions
        .entries
        .get_mut("std-email")
        .expect("extension")
        .state = ExtensionState::Spawning;

    h.handle_extension_event(
        "std-email",
        TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("tau-ext-pim"),
            client_kind: tau_proto::ClientKind::Tool,
            expected_session_id: None,
            capabilities: Default::default(),
        })),
    )
    .expect("hello");

    let frames = sink.lock().expect("sink");
    let configure = frames
        .iter()
        .find_map(|routed| match &routed.frame {
            HarnessOutputMessage::Configure(configure) => Some(configure),
            _ => None,
        })
        .expect("configure sent");
    let expected =
        tau_config::settings::extension_state_dir_of(&sp, "std-email").expect("safe name");
    assert_eq!(configure.state_dir.as_deref(), Some(expected.as_path()));
    assert!(expected.is_dir(), "{} should exist", expected.display());
}

/// Proves a stale providers directory for a tool extension cannot cross
/// the provider-only Configure boundary, even for a supervised persistent peer.
#[test]
fn tool_configure_ignores_stale_provider_settings_directory() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let settings = tau_config::settings::extension_provider_settings_dir_of(&sp, "std-email")
        .expect("settings path");
    std::fs::create_dir_all(&settings).expect("stale settings directory");
    std::fs::write(settings.join("provider.json"), b"preview-settings").expect("settings");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let configure =
        configure_supervised_extension(&mut h, "std-email", tau_proto::ClientKind::Tool);
    assert!(configure.settings_files.is_empty());
}

/// Memory-only lifecycle still completes Hello/Configure while delegating no
/// persistent extension state path.
#[test]
fn memory_only_configure_omits_extension_state_dir() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness_with_start_reason_and_storage_mode(
        &sp,
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::MemoryOnly,
    )
    .expect("memory-only harness");
    let sink = connect_handshaking_tool(&mut h, "std-email");
    h.extensions
        .entries
        .get_mut("std-email")
        .expect("extension")
        .state = ExtensionState::Spawning;

    h.handle_extension_event(
        "std-email",
        TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("tau-ext-pim"),
            client_kind: tau_proto::ClientKind::Tool,
            expected_session_id: None,
            capabilities: Default::default(),
        })),
    )
    .expect("hello");

    let frames = sink.lock().expect("sink");
    let configure = frames
        .iter()
        .find_map(|routed| match &routed.frame {
            HarnessOutputMessage::Configure(configure) => Some(configure),
            _ => None,
        })
        .expect("configure sent");
    assert_eq!(configure.state_dir, None);
    assert!(!sp.exists());
}

#[test]
fn configure_includes_only_resolved_extension_secrets() {
    // The lifecycle handshake is the authorization boundary for extension
    // secrets: only the resolved map stored on that extension entry is sent.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let sink = connect_handshaking_tool(&mut h, "std-email");
    h.extensions
        .entries
        .get_mut("std-email")
        .expect("extension entry")
        .secrets
        .insert(
            "mail_password".to_owned(),
            tau_proto::SecretValue::new("secret"),
        );
    h.extensions
        .entries
        .get_mut("std-email")
        .expect("extension")
        .state = ExtensionState::Spawning;

    h.handle_extension_event(
        "std-email",
        TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("tau-ext-pim"),
            client_kind: tau_proto::ClientKind::Tool,
            expected_session_id: None,
            capabilities: Default::default(),
        })),
    )
    .expect("hello");

    let frames = sink.lock().expect("sink");
    let configure = frames
        .iter()
        .find_map(|routed| match &routed.frame {
            HarnessOutputMessage::Configure(configure) => Some(configure),
            _ => None,
        })
        .expect("configure sent");
    assert_eq!(configure.secrets.len(), 1);
    assert_eq!(configure.secrets["mail_password"].expose_secret(), "secret");
}

/// Proves the lifecycle checks the complete encoded Configure frame, including
/// non-settings fields, before handing it to the protocol writer.
#[test]
fn oversized_complete_configure_disconnects_before_publication() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let sink = connect_handshaking_tool(&mut h, "oversized-configure");
    let entry = h
        .extensions
        .entries
        .get_mut("oversized-configure")
        .expect("extension");
    entry.secrets.insert(
        "oversized".to_owned(),
        tau_proto::SecretValue::new("x".repeat(
            usize::try_from(tau_proto::MAX_PROTOCOL_MESSAGE_BYTES).expect("frame size") + 1,
        )),
    );
    entry.state = ExtensionState::Spawning;

    h.handle_extension_event(
        "oversized-configure",
        TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("oversized-configure"),
            client_kind: tau_proto::ClientKind::Tool,
            expected_session_id: None,
            capabilities: Default::default(),
        })),
    )
    .expect("hello");

    let frames = sink.lock().expect("sink");
    assert!(frames.iter().any(|routed| {
        matches!(
            &routed.frame,
            HarnessOutputMessage::Disconnect(Disconnect { reason: Some(reason) })
                if reason == "extension Configure exceeds protocol frame limit"
        )
    }));
    assert!(
        !frames
            .iter()
            .any(|routed| matches!(routed.frame, HarnessOutputMessage::Configure(_)))
    );
}

#[test]
fn extension_config_error_is_mandatory_warning_and_replayed_to_late_ui() {
    // Extension config validation often runs during daemon startup, before the
    // terminal UI has subscribed. This is regression coverage for the user
    // contract: any extension `ConfigError` must become a mandatory warning
    // `harness.notice` visible to late UI clients, not just a debug-log line.
    // Keep the mandatory diagnostic replay-visible for late UI subscribers.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "config-bad-ext";
    let _extension_sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::ConfigError(tau_proto::ConfigError {
            message: "unknown field `enforce_ro_mode`".to_owned(),
        }),
    )
    .expect("config error handled");

    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_CONFIG_ERROR
                    && info.purpose == tau_proto::NoticePurpose::Alert
                    && info.message.contains("extension config-bad-ext rejected its config")
                    && info.message.contains("unknown field `enforce_ro_mode`")
        )
    ));

    let ui_conn: tau_proto::ConnectionId = crate::test_connection_id("late-ui");
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
                && info.kind == tau_proto::notice_kind::EXTENSION_CONFIG_ERROR
                && info.purpose == tau_proto::NoticePurpose::Alert
                && info.message.contains("extension config-bad-ext rejected its config")
                && info.message.contains("unknown field `enforce_ro_mode`")
    )));
}

/// A committed bridge report produces a durable canonical fact with
/// authenticated configured-name provenance.
#[test]
fn extension_message_report_produces_stamped_durable_fact() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let conn_id = "bridge-main";
    let configured_name = "configured-bridge";
    connect_handshaking_tool(&mut h, conn_id);
    let entry = h.extensions.entries.get_mut(conn_id).expect("extension");
    entry.state = ExtensionState::Ready;
    entry.name = crate::test_extension_name(configured_name);
    entry
        .peer_capabilities
        .insert(tau_proto::PeerCapability::MessageBridge);
    let observer = connect_test_client(&mut h, "observer", tau_proto::ClientKind::Ui);
    h.handle_client_event(
        "observer",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Prefix("message.".to_owned())],
        })),
    )
    .expect("subscribe");
    let report = tau_proto::MessageDelivered {
        publisher_extension_id: tau_proto::RawMessagePublisherId::new(configured_name),
        agent_id: tau_proto::MessageAgentTarget::new("missing-agent"),
        message_id: tau_proto::MessageFactId::new("m1"),
        sender: tau_proto::MessageParty {
            stable_id: "u1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        conversation: None,
        text: "hello".to_owned(),
        extension_data: tau_proto::MessageExtensionData::default(),
    };

    let canonical = Event::MessageDeliveredReported(report.clone())
        .into_stamped_canonical_message_fact(
            tau_proto::MessagePublisherId::parse(configured_name)
                .expect("canonical publisher id must satisfy the identifier grammar"),
        )
        .expect("authenticated message fact");
    assert!(matches!(
        canonical,
        Event::MessageDelivered(prepared)
            if prepared.publisher_extension_id.as_str() == configured_name
    ));

    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id(conn_id),
        Event::MessageDeliveredReported(report),
        Some(false),
    )
    .expect("report intake");

    assert!(matches!(
        h.session_runtime.store
            .session_events("s1")
            .expect("durable fallback journal")
            .as_slice(),
        [tau_core::PersistedSessionEvent {
            source: Some(source),
            event: Event::MessageDelivered(fact),
            ..
        }] if source.connection_id().is_some_and(|source| {
            source == crate::harness::harness_connection_id()
        })
            && fact.publisher_extension_id.as_str() == configured_name
    ));
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::MessageDelivered(fact)
                if fact.publisher_extension_id.as_str() == configured_name
        )
    ));
    assert!(event_log_contains_source_event(&h, conn_id, |event| {
        matches!(
            event,
            Event::MessageDeliveredReported(report)
                if report.publisher_extension_id.as_str() == configured_name
        )
    }));
    let events = event_log_events(&h);
    let report_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::MessageDeliveredReported(report)
                    if report.publisher_extension_id.as_str() == configured_name
            )
        })
        .expect("committed report");
    let canonical_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::MessageDelivered(fact)
                    if fact.publisher_extension_id.as_str() == configured_name
            )
        })
        .expect("canonical fact");
    assert!(report_index < canonical_index);
    let live_names = observer
        .lock()
        .expect("observer")
        .iter()
        .filter_map(|frame| peel_inner_event(&frame.frame).map(|event| event.name()))
        .filter(|name| name.category() == &tau_proto::EventCategory::Message)
        .collect::<Vec<_>>();
    assert_eq!(
        live_names,
        [
            tau_proto::EventName::MESSAGE_DELIVERED_REPORTED,
            tau_proto::EventName::MESSAGE_DELIVERED,
        ]
    );
}

/// Invalid or mismatched raw publisher claims remain observable reports but do
/// not produce canonical durable facts.
#[test]
fn extension_message_report_requires_exact_authenticated_publisher_claim() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let conn_id = "bridge-main";
    let configured_name = "configured-bridge";
    connect_handshaking_tool(&mut h, conn_id);
    let entry = h.extensions.entries.get_mut(conn_id).expect("extension");
    entry.state = ExtensionState::Ready;
    entry.name = crate::test_extension_name(configured_name);
    entry
        .peer_capabilities
        .insert(tau_proto::PeerCapability::MessageBridge);

    for claim in [
        "other-bridge",
        "bad publisher",
        "control\u{1f}",
        "c1\u{80}",
        "unicode-☃",
    ] {
        let report = tau_proto::MessageDelivered {
            publisher_extension_id: tau_proto::RawMessagePublisherId::new(claim),
            agent_id: tau_proto::MessageAgentTarget::new("missing-agent"),
            message_id: tau_proto::MessageFactId::new("m1"),
            sender: tau_proto::MessageParty {
                stable_id: "u1".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            conversation: None,
            text: "hello".to_owned(),
            extension_data: tau_proto::MessageExtensionData::default(),
        };
        h.handle_extension_event_inner_with_persist(
            &crate::test_connection_id(conn_id),
            Event::MessageDeliveredReported(report),
            Some(false),
        )
        .expect("report intake");
        assert!(event_log_contains_source_event(&h, conn_id, |event| {
            matches!(
                event,
                Event::MessageDeliveredReported(report)
                    if report.publisher_extension_id.as_str() == claim
            )
        }));
    }

    assert!(
        h.session_runtime
            .store
            .session_events("s1")
            .expect("durable fallback journal")
            .is_empty()
    );
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::MessageDelivered(fact)
                if fact.publisher_extension_id.as_str() == configured_name
        )
    }));
}

/// A configured extension cannot bypass report processing by directly emitting
/// a harness-owned canonical message fact.
#[test]
fn extension_cannot_emit_canonical_message_fact() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    connect_ready_message_publisher(&mut h, "bridge", "configured-bridge");
    let canonical = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("forged").expect("canonical publisher"),
        tau_proto::MessageAgentTarget::new("missing-agent"),
        tau_proto::MessageFactId::new("m1"),
        tau_proto::MessageParty {
            stable_id: "u1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "hello",
    ));

    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("bridge"),
        canonical,
        Some(true),
    )
    .expect("extension intake");

    assert!(
        h.session_runtime
            .store
            .session_events("s1")
            .expect("fallback journal")
            .is_empty()
    );
    assert!(event_log_events(&h).iter().all(|event| {
        !matches!(
            event,
            Event::MessageDelivered(fact) if fact.message_id.as_str() == "m1"
        )
    }));
}

/// A configured extension needs the declared message-bridge capability before
/// it can submit a report.
#[test]
fn ordinary_tool_and_provider_extensions_cannot_emit_message_reports() {
    for (connection_id, kind) in [
        ("tool", tau_proto::ClientKind::Tool),
        ("provider", tau_proto::ClientKind::Provider),
    ] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        connect_handshaking_extension(&mut h, connection_id, kind);
        h.extensions
            .entries
            .get_mut(connection_id)
            .expect("extension")
            .state = ExtensionState::Ready;
        h.handle_extension_event_inner_with_persist(
            &crate::test_connection_id(connection_id),
            Event::MessageDeliveredReported(tau_proto::MessageDelivered::new(
                tau_proto::RawMessagePublisherId::new("forged"),
                tau_proto::MessageAgentTarget::new("missing-agent"),
                tau_proto::MessageFactId::new("m1"),
                tau_proto::MessageParty {
                    stable_id: "u1".to_owned(),
                    display_name: None,
                    sender_auth: None,
                },
                None,
                "hello",
            )),
            Some(false),
        )
        .expect("report intake");
        assert!(event_log_events(&h).iter().all(|event| {
            !matches!(
                event,
                Event::MessageDelivered(_) | Event::MessageDeliveredReported(_)
            )
        }));
    }
}

/// Extensions cannot skip the Hello/Configure gate by declaring capabilities
/// or announcing readiness while their lifecycle state is still Spawning.
#[test]
fn extension_rejects_pre_hello_protocol_messages() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    let conn_id = "pre-hello-ext";
    let _extension_sink = connect_handshaking_tool(&mut h, conn_id);
    h.extensions
        .entries
        .get_mut(conn_id)
        .expect("extension entry")
        .state = ExtensionState::Spawning;

    let declaration_error = h
        .handle_extension_event(
            conn_id,
            TestProtocolItem::Event(Event::ToolRegistrationDeclared(
                tau_proto::ToolRegistrationDeclared {
                    tool: staged_tool_spec("too_early"),
                    tool_group: None,
                    prompt_fragment: None,
                },
            )),
        )
        .expect_err("pre-Hello declaration must fail");
    assert!(declaration_error.to_string().contains("out-of-order"));
    assert!(
        h.tool_routing
            .registry
            .providers_for("too_early")
            .is_empty()
    );

    let ready_error = h
        .handle_extension_message(
            &crate::test_connection_id(conn_id),
            TestMessage::Ready(Default::default()),
        )
        .expect_err("pre-Hello Ready must fail");
    assert!(ready_error.to_string().contains("out-of-order"));
}

/// Covers optional-startup availability and replay required by
/// `SPEC-tau-harness-extension-lifecycle`.
#[test]
fn optional_extension_config_error_is_replayed_and_disables_extension() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    let conn_id = "optional-config-bad-ext";
    let _extension_sink = connect_handshaking_tool(&mut h, conn_id);
    h.extensions
        .entries
        .get_mut(conn_id)
        .expect("extension entry")
        .require = false;

    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::ConfigError(tau_proto::ConfigError {
            message: "missing token".to_owned(),
        }),
    )
    .expect("config error handled");

    let entry = h.extensions.entries.get(conn_id).expect("extension entry");
    assert_eq!(entry.state, ExtensionState::Disconnected);
    assert!(!entry.respawn_allowed);
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.message.contains("extension optional-config-bad-ext rejected its config")
                    && info.message.contains("missing token")
        )
    ));
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                    && info.purpose == tau_proto::NoticePurpose::Alert
                    && info.message == "optional extension optional-config-bad-ext did not initialize"
        )
    ));

    let ui_conn: tau_proto::ConnectionId = crate::test_connection_id("late-ui-optional-config");
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
                && info.message.contains("extension optional-config-bad-ext rejected its config")
                && info.message.contains("missing token")
    )));
    assert!(frames.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::HarnessNotice(info))
            if info.level == tau_proto::NoticeLevel::Warning
                && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                && info.purpose == tau_proto::NoticePurpose::Alert
                && info.message == "optional extension optional-config-bad-ext did not initialize"
    )));
}

/// Ensures an optional spawn failure remains nonfatal while its mandatory
/// replayable notice carries only bounded, secret-safe diagnostic context.
#[test]
fn optional_extension_spawn_failure_is_mandatory_warning_and_nonfatal() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let extension_name = "optional-spawn-failure".to_owned();
    let command = format!("/definitely/not/a/{}-trailing-secret", "y".repeat(300));
    let config = crate::settings::Config {
        extensions: BTreeMap::from([(
            extension_name.clone(),
            crate::settings::ExtensionConfig {
                tool_prefix: None,
                name: extension_name.clone(),
                command,
                args: vec!["--token=argument-secret".to_owned()],
                role: None,
                component: None,
                require: false,
                startup_timeout: Duration::from_secs(2),
                cwd: None,
                config: serde_json::json!({"token": "config-secret"}),
                secrets: BTreeMap::new(),
                tau_state_access: TauStateAccess::Legacy,
                tau_runtime_socket_access: TauRuntimeSocketAccess::Hidden,
            },
        )]),
        extension_startup_diagnostics: Vec::new(),
        harness_settings: HarnessSettings::built_in(),
    };
    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);

    h.spawn_configured_extensions(
        &config,
        &sessions_dir,
        "s1",
        &BTreeMap::new(),
        &BTreeSet::new(),
        Instant::now(),
    )
    .expect("optional spawn failure should not fail startup");

    assert!(h.extension_connection_id(&extension_name).is_none());
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                    && info.purpose == tau_proto::NoticePurpose::Alert
                    && info.message.contains("failed to spawn configured extension instance")
                    && info.message.contains("`command` executable")
                    && info.message.contains('…')
                    && info.message.len() < 1_500
                    && !info.message.contains("trailing-secret")
                    && !info.message.contains("argument-secret")
                    && !info.message.contains("config-secret")
                    && !info.message.contains("cwd")
        )
    ));
}

#[test]
fn optional_pre_ready_disconnect_is_mandatory_warning_replayed_and_nonfatal() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "optional-pre-ready-drop";
    let _extension_sink = connect_handshaking_tool(&mut h, conn_id);
    h.extensions
        .entries
        .get_mut(conn_id)
        .expect("extension entry")
        .require = false;

    h.handle_startup_disconnect(&crate::test_connection_id(conn_id))
        .expect("optional pre-ready disconnect should not fail startup");

    let entry = h.extensions.entries.get(conn_id).expect("extension entry");
    assert_eq!(entry.state, ExtensionState::Disconnected);
    assert!(!entry.respawn_allowed);
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                    && info.purpose == tau_proto::NoticePurpose::Alert
                    && info.message == "optional extension optional-pre-ready-drop did not initialize"
        )
    ));

    let ui_conn: tau_proto::ConnectionId = crate::test_connection_id("late-ui-pre-ready-drop");
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
                && info.message == "optional extension optional-pre-ready-drop did not initialize"
    )));
}

/// Ensures a required extension's configured deadline fails only that expired
/// peer, leaving a concurrently initializing peer with a later deadline out of
/// the startup diagnostic.
#[test]
fn required_extension_startup_deadlines_are_independent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let expired = crate::test_connection_id("expired-required");
    let later = crate::test_connection_id("later-required");
    let _expired_sink = connect_handshaking_tool(&mut h, expired.as_str());
    let _later_sink = connect_handshaking_tool(&mut h, later.as_str());
    let now = Instant::now();
    h.extensions.startup_deadlines.insert(
        expired.clone(),
        StartupDeadline {
            deadline: now,
            name: crate::test_extension_name("expired-required"),
            require: true,
        },
    );
    h.extensions.startup_deadlines.insert(
        later.clone(),
        StartupDeadline {
            deadline: now + Duration::from_secs(10),
            name: crate::test_extension_name("later-required"),
            require: true,
        },
    );

    let error = h
        .handle_expired_extension_startup_deadlines(now)
        .expect_err("expired required extension fails startup");

    assert!(matches!(error, HarnessError::StartupTimeout));
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.message.contains("expired-required")
                    && !info.message.contains("later-required")
        )
    ));
    assert_eq!(
        h.extensions.startup_deadlines.get(&later),
        Some(&StartupDeadline {
            deadline: now + Duration::from_secs(10),
            name: crate::test_extension_name("later-required"),
            require: true,
        })
    );
}

/// Ensures an externally managed queued peer retains its one startup-wait
/// deadline through unrelated events rather than receiving a fresh deadline for
/// every event the harness processes.
#[test]
fn required_pending_external_extension_deadline_survives_event_churn() {
    fn dormant_extension(mut reader: UnixStream, _writer: UnixStream) -> Result<(), String> {
        let mut byte = [0_u8; 1];
        let _ = reader.read(&mut byte).map_err(|error| error.to_string())?;
        Ok(())
    }

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let spawned = spawn_in_process(
        "queued-required",
        tau_proto::ClientKind::Tool,
        dormant_extension,
        &h.runtime_io.tx,
        &h.runtime_io.component_ingress_tx,
    )
    .expect("spawn queued extension");
    let connection_id = spawned.connection_id.clone();
    let (unrelated_client, _unrelated_peer) = UnixStream::pair().expect("unrelated client");
    h.runtime_io
        .tx
        .send(HarnessEvent::NewClient(unrelated_client))
        .expect("queue unrelated event");
    h.queue_extension_connect(ExtensionConnectCommand {
        entry: ExtensionEntry {
            tool_prefix: None,
            name: crate::test_extension_name("queued-required"),
            instance_id: 701.into(),
            connection_id: connection_id.clone(),
            kind: tau_proto::ClientKind::Tool,
            peer_capabilities: Default::default(),
            require: true,
            respawn_allowed: false,
            pid: Some(std::process::id()),
            in_process_thread: Some(spawned.thread),
            supervised_config: None,
            secrets: BTreeMap::new(),
            restart_attempt: 0,
            state: ExtensionState::Spawning,
            protocol_io: spawned.protocol_io,
        },
        origin: ConnectionOrigin::InMemory,
        writer_tx: spawned.writer_tx,
        initialized_ack: spawned.initialized_ack,
        supervised_writer: None,
        replaces: None,
    })
    .expect("queue extension");

    let error = h
        .wait_for_extensions_ready_at(Instant::now() - Duration::from_secs(3))
        .expect_err("expired pending required extension fails startup");

    assert!(matches!(error, HarnessError::StartupTimeout));
    assert!(h.extensions.entries.contains_key(&connection_id));
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info) if info.message.contains("queued-required")
        )
    ));
}

/// Ensures authenticated Ready classification uses its decode observation:
/// D-minus-epsilon and D are accepted after queued handling, while
/// D-plus-epsilon remains fail-closed.
#[test]
fn ready_decode_observation_owns_extension_startup_deadline_boundary() {
    let epsilon = Duration::from_nanos(1);
    for (name, offset, accepted) in [
        ("before", Some(epsilon), true),
        ("at", None, true),
        ("after", Some(epsilon), false),
    ] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        let connection_id = crate::test_connection_id(format!("deadline-ready-{name}"));
        let _sink = connect_handshaking_tool(&mut h, connection_id.as_str());
        let deadline = Instant::now() - Duration::from_secs(1);
        h.extensions.startup_deadlines.insert(
            connection_id.clone(),
            StartupDeadline {
                deadline,
                name: crate::test_extension_name(format!("deadline-ready-{name}")),
                require: true,
            },
        );
        let decoded_at = match (accepted, offset) {
            (true, Some(offset)) => deadline - offset,
            (true, None) => deadline,
            (false, Some(offset)) => deadline + offset,
            (false, None) => unreachable!("late case has an offset"),
        };
        h.runtime_io
            .tx
            .send(HarnessEvent::from_connection_observed_at_for_test(
                connection_id.clone(),
                HarnessInputMessage::Ready(Default::default()),
                decoded_at,
            ))
            .expect("queue ready");

        let result = h.wait_for_extensions_ready_at(deadline - STARTUP_TIMEOUT);
        if accepted {
            result.expect("on-time decoded Ready activates after queued handling");
            assert_eq!(
                h.extensions.entries[&connection_id].state,
                ExtensionState::Ready
            );
            assert!(!h.extensions.startup_deadlines.contains_key(&connection_id));
        } else {
            assert!(matches!(result, Err(HarnessError::StartupTimeout)));
            assert_eq!(
                h.extensions.entries[&connection_id].state,
                ExtensionState::Handshaking
            );
            assert!(!h.extensions.ready_received.contains(&connection_id));
        }
    }
}

/// Ensures an on-time Ready blocked behind its own decoded Hello authenticates
/// before expiry and then activates through the real bounded ingress lane.
#[test]
fn ready_blocked_behind_hello_retains_decode_deadline_authority() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let connection_id = crate::test_connection_id("queued-hello-ready");
    let _sink = connect_handshaking_tool(&mut h, connection_id.as_str());
    h.extensions
        .entries
        .get_mut(&connection_id)
        .expect("extension entry")
        .state = ExtensionState::Spawning;
    let deadline = Instant::now() - Duration::from_secs(1);
    h.extensions.startup_deadlines.insert(
        connection_id.clone(),
        StartupDeadline {
            deadline,
            name: crate::test_extension_name("queued-hello-ready"),
            require: true,
        },
    );
    let (unrelated_server, _unrelated_peer) = UnixStream::pair().expect("unrelated pair");
    h.runtime_io
        .tx
        .send(HarnessEvent::NewClient(unrelated_server))
        .expect("queue unrelated event ahead of Hello");
    h.runtime_io
        .component_ingress_tx
        .send_for_test(HarnessEvent::from_connection_observed_at_for_test(
            connection_id.clone(),
            HarnessInputMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: crate::test_extension_name("queued-hello-ready"),
                client_kind: tau_proto::ClientKind::Tool,
                expected_session_id: None,
                capabilities: Default::default(),
            }),
            deadline - Duration::from_nanos(2),
        ))
        .expect("occupy ingress with Hello");
    let ready_sender = h.runtime_io.component_ingress_tx.clone();
    let ready_connection = connection_id.clone();
    let ready = std::thread::spawn(move || {
        ready_sender.send_for_test(HarnessEvent::from_connection_observed_at_for_test(
            ready_connection,
            HarnessInputMessage::Ready(Default::default()),
            deadline - Duration::from_nanos(1),
        ))
    });
    h.runtime_io.component_ingress.wait_for_blocked_sender();

    h.wait_for_extensions_ready_at(deadline - STARTUP_TIMEOUT)
        .expect("decoded Hello and Ready retain deadline authority");

    assert_eq!(
        h.extensions.entries[&connection_id].state,
        ExtensionState::Ready
    );
    assert!(!h.extensions.startup_deadlines.contains_key(&connection_id));
    assert_eq!(ready.join().expect("Ready sender joins"), Ok(()));
}

/// Ensures one pending on-time Ready protects only its exact connection and
/// cannot defer a silent required peer's earlier independent deadline.
#[test]
fn pending_ready_does_not_extend_silent_peer_startup_deadline() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let silent = crate::test_connection_id("silent-required");
    let pending = crate::test_connection_id("pending-ready");
    let _silent_sink = connect_handshaking_tool(&mut h, silent.as_str());
    let _pending_sink = connect_handshaking_tool(&mut h, pending.as_str());
    let now = Instant::now();
    h.extensions.startup_deadlines.insert(
        silent.clone(),
        StartupDeadline {
            deadline: now - Duration::from_secs(1),
            name: crate::test_extension_name("silent-required"),
            require: true,
        },
    );
    h.extensions.startup_deadlines.insert(
        pending.clone(),
        StartupDeadline {
            deadline: now + Duration::from_secs(10),
            name: crate::test_extension_name("pending-ready"),
            require: true,
        },
    );
    let (admitted_rx, wake_release) = h.runtime_io.component_ingress.pause_next_wake_for_test();
    let pending_sender = h.runtime_io.component_ingress_tx.clone();
    let pending_connection = pending.clone();
    let ready = std::thread::spawn(move || {
        pending_sender.send_for_test(HarnessEvent::from_connection_observed_at_for_test(
            pending_connection,
            HarnessInputMessage::Ready(Default::default()),
            now,
        ))
    });
    admitted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("pending Ready admitted before wake");
    let (classified_tx, classified_rx) = mpsc::channel();
    let release = std::thread::spawn(move || {
        let classified = classified_rx.recv_timeout(Duration::from_secs(1));
        wake_release.send(()).expect("release pending Ready wake");
        classified
    });

    let result = h.wait_for_extensions_ready_at(now - STARTUP_TIMEOUT);
    let _ = classified_tx.send(());

    assert!(matches!(result, Err(HarnessError::StartupTimeout)));
    release
        .join()
        .expect("release helper joins")
        .expect("silent peer classifies before pending Ready wake");
    assert_eq!(ready.join().expect("Ready sender joins"), Ok(()));
    assert_eq!(
        h.extensions.entries[&silent].state,
        ExtensionState::Handshaking
    );
    assert_eq!(
        h.extensions.entries[&pending].state,
        ExtensionState::Handshaking
    );
    assert!(h.extensions.startup_deadlines.contains_key(&pending));
}

/// Ensures an on-time Ready admitted before its ingress wake still wins when
/// the startup receive deadline expires while the producer is descheduled.
#[test]
fn ready_admitted_before_delayed_wake_retains_decode_deadline_authority() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let connection_id = crate::test_connection_id("ready-before-wake");
    let _sink = connect_handshaking_tool(&mut h, connection_id.as_str());
    let deadline = Instant::now() + Duration::from_millis(20);
    h.extensions.startup_deadlines.insert(
        connection_id.clone(),
        StartupDeadline {
            deadline,
            name: crate::test_extension_name("ready-before-wake"),
            require: true,
        },
    );
    let (admitted_rx, wake_release) = h.runtime_io.component_ingress.pause_next_wake_for_test();
    let (observed_rx, observation_release) = h
        .runtime_io
        .component_ingress
        .pause_next_observation_for_test();
    let sender = h.runtime_io.component_ingress_tx.clone();
    let ready = std::thread::spawn(move || {
        sender.send_for_test(HarnessEvent::from_connection_observed_at_for_test(
            connection_id,
            HarnessInputMessage::Ready(Default::default()),
            deadline - Duration::from_nanos(1),
        ))
    });
    admitted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Ready admitted before wake");

    let release = std::thread::spawn(move || {
        let observed = observed_rx.recv_timeout(Duration::from_secs(1));
        observation_release
            .send(())
            .expect("release startup observation");
        wake_release.send(()).expect("release ingress wake");
        observed
    });
    let result = h.wait_for_extensions_ready_at(deadline - STARTUP_TIMEOUT);

    release
        .join()
        .expect("release helper joins")
        .expect("waiter rechecks admitted Ready after receive timeout");
    result.expect("admitted on-time Ready survives delayed wake");
    assert_eq!(ready.join().expect("Ready sender joins"), Ok(()));
}

/// Ensures the initial authenticated UI Subscribe uses the same exact decode
/// boundary without allowing a late Subscribe to revive startup.
#[test]
fn initial_ui_subscribe_decode_observation_owns_startup_deadline_boundary() {
    let epsilon = Duration::from_nanos(1);
    for (offset, accepted) in [(Some(epsilon), true), (None, true), (Some(epsilon), false)] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        let (server, _client) = UnixStream::pair().expect("initial UI pair");
        let connection_id = h.accept_client(server).expect("accept initial UI");
        let started_at = Instant::now() - Duration::from_secs(3);
        let deadline = started_at + STARTUP_TIMEOUT;
        h.runtime_io
            .tx
            .send(HarnessEvent::from_connection_observed_at_for_test(
                connection_id.clone(),
                HarnessInputMessage::Hello(tau_proto::Hello {
                    protocol_version: tau_proto::PROTOCOL_VERSION,
                    client_name: crate::test_extension_name("deadline-ui"),
                    client_kind: tau_proto::ClientKind::Ui,
                    expected_session_id: None,
                    capabilities: Default::default(),
                }),
                started_at,
            ))
            .expect("queue initial UI Hello");
        let decoded_at = match (accepted, offset) {
            (true, Some(offset)) => deadline - offset,
            (true, None) => deadline,
            (false, Some(offset)) => deadline + offset,
            (false, None) => unreachable!("late case has an offset"),
        };
        h.runtime_io
            .tx
            .send(HarnessEvent::from_connection_observed_at_for_test(
                connection_id,
                HarnessInputMessage::Subscribe(tau_proto::Subscribe {
                    historical_selectors: Vec::new(),
                    live_selectors: Vec::new(),
                }),
                decoded_at,
            ))
            .expect("queue initial UI Subscribe");

        let result = h.wait_for_initial_ui_subscribe_at(started_at);
        if accepted {
            result.expect("on-time decoded Subscribe survives queued handling");
        } else {
            assert!(matches!(result, Err(HarnessError::StartupTimeout)));
        }
    }
}

/// Ensures an on-time authenticated initial Subscribe admitted before its
/// ingress wake still wins when the receive deadline expires first.
#[test]
fn initial_subscribe_admitted_before_delayed_wake_retains_decode_authority() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let (server, _client) = UnixStream::pair().expect("initial UI pair");
    let connection_id = h.accept_client(server).expect("accept initial UI");
    assert!(
        !h.handle_startup_from_connection(
            &connection_id,
            HarnessInputMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: crate::test_extension_name("subscribe-before-wake"),
                client_kind: tau_proto::ClientKind::Ui,
                expected_session_id: None,
                capabilities: Default::default(),
            }),
        )
        .expect("authenticate initial UI")
    );
    let deadline = Instant::now() + Duration::from_millis(20);
    let (admitted_rx, wake_release) = h.runtime_io.component_ingress.pause_next_wake_for_test();
    let (observed_rx, observation_release) = h
        .runtime_io
        .component_ingress
        .pause_next_observation_for_test();
    let sender = h.runtime_io.component_ingress_tx.clone();
    let subscribe = std::thread::spawn(move || {
        sender.send_for_test(HarnessEvent::from_connection_observed_at_for_test(
            connection_id,
            HarnessInputMessage::Subscribe(tau_proto::Subscribe {
                historical_selectors: Vec::new(),
                live_selectors: Vec::new(),
            }),
            deadline - Duration::from_nanos(1),
        ))
    });
    admitted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Subscribe admitted before wake");

    let release = std::thread::spawn(move || {
        let observed = observed_rx.recv_timeout(Duration::from_secs(1));
        observation_release
            .send(())
            .expect("release startup observation");
        wake_release.send(()).expect("release ingress wake");
        observed
    });
    let result = h.wait_for_initial_ui_subscribe_at(deadline - STARTUP_TIMEOUT);

    release
        .join()
        .expect("release helper joins")
        .expect("waiter rechecks admitted Subscribe after receive timeout");
    result.expect("admitted on-time Subscribe survives delayed wake");
    assert_eq!(subscribe.join().expect("Subscribe sender joins"), Ok(()));
}

/// Ensures an optional peer that expires before its queued connect installs is
/// disconnected on install, while a later peer reaches Ready and closes the
/// initial activation barrier.
#[test]
fn expired_optional_pending_extension_is_disabled_without_blocking_later_ready() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let later = crate::test_connection_id("later-ready");
    let _later_sink = connect_handshaking_tool(&mut h, later.as_str());
    let mut config = supervised_test_config("queued-optional", "exec sleep 60");
    config.require = false;
    let spawned = spawn_supervised(
        &config,
        tau_proto::ClientKind::Tool,
        None,
        &h.runtime_io.tx,
        &h.runtime_io.component_ingress_tx,
        &h.session_runtime.state_dir,
        h.session_runtime.storage_mode.is_memory_only(),
        &Default::default(),
    )
    .expect("spawn optional extension");
    let optional = spawned.connection_id.clone();
    h.queue_extension_connect(ExtensionConnectCommand {
        entry: ExtensionEntry {
            tool_prefix: None,
            name: crate::test_extension_name("queued-optional"),
            instance_id: 702.into(),
            connection_id: optional.clone(),
            kind: tau_proto::ClientKind::Tool,
            peer_capabilities: Default::default(),
            require: false,
            respawn_allowed: false,
            pid: Some(spawned.child_pid),
            in_process_thread: None,
            supervised_config: Some(config),
            secrets: BTreeMap::new(),
            restart_attempt: 0,
            state: ExtensionState::Spawning,
            protocol_io: spawned.protocol_io,
        },
        origin: ConnectionOrigin::Supervised,
        writer_tx: spawned.writer_tx,
        initialized_ack: spawned.initialized_ack,
        supervised_writer: Some(spawned.writer),
        replaces: None,
    })
    .expect("queue optional extension");
    let now = Instant::now();
    h.extensions.startup_deadlines.insert(
        optional.clone(),
        StartupDeadline {
            deadline: now,
            name: crate::test_extension_name("queued-optional"),
            require: false,
        },
    );
    h.extensions.startup_deadlines.insert(
        later.clone(),
        StartupDeadline {
            deadline: now + Duration::from_secs(10),
            name: crate::test_extension_name("later-ready"),
            require: true,
        },
    );
    h.runtime_io
        .tx
        .send(HarnessEvent::from_connection_for_test(
            later.clone(),
            HarnessInputMessage::Ready(Default::default()),
        ))
        .expect("queue later Ready");

    h.wait_for_extensions_ready_at(now)
        .expect("optional expiry is nonfatal");

    assert_eq!(
        h.extensions.entries[&optional].state,
        ExtensionState::Disconnected
    );
    assert_eq!(h.extensions.entries[&later].state, ExtensionState::Ready);
    assert!(!h.extensions.startup_deadlines.contains_key(&optional));
    assert!(!h.extensions.startup_deadlines.contains_key(&later));
    assert!(h.extensions.initial_tool_preflight_complete);
    assert!(h.extensions.supervised_writers.contains_key(&optional));
    assert!(h.extensions.cleanup_deadlines.contains_key(&optional));
    h.shutdown()
        .expect("supervised optional child is reaped during bounded shutdown");
}

#[test]
fn post_ready_optional_tool_disconnect_keeps_existing_respawn_policy_flag() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "optional-ready-tool";
    let _sink = connect_handshaking_tool(&mut h, conn_id);
    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("ready");
    {
        let entry = h
            .extensions
            .entries
            .get_mut(conn_id)
            .expect("extension entry");
        entry.require = false;
        entry.respawn_allowed = true;
    }

    h.handle_disconnect(&crate::test_connection_id(conn_id));

    let entry = h.extensions.entries.get(conn_id).expect("extension entry");
    assert_eq!(entry.state, ExtensionState::Disconnected);
    assert!(entry.respawn_allowed);
}

#[test]
fn handshaking_tool_register_is_not_active_before_ready() {
    // Capability staging: a tool announced during handshake must not enter the
    // live registry, prompt tool list, or prompt fragments until the extension
    // sends Ready. Tests bypass dispatch gating to verify the assembly inputs
    // directly.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let conn_id = "conn-staged-before-ready";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("staged_tool"),
                tool_group: None,
                prompt_fragment: Some(tau_proto::PromptFragment::new(
                    "staged_tool.instructions",
                    tau_proto::PromptPriority::new(10),
                    "STAGED TOOL PROMPT",
                )),
            },
        )),
    )
    .expect("stage tool");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtPromptFragmentPublish(
            tau_proto::ExtPromptFragmentPublish {
                fragment: tau_proto::PromptFragment::new(
                    "staged.extension.instructions",
                    tau_proto::PromptPriority::new(20),
                    "STAGED EXTENSION PROMPT",
                ),
            },
        )),
    )
    .expect("stage extension prompt fragment");

    assert!(
        h.tool_routing
            .registry
            .providers_for("staged_tool")
            .is_empty()
    );
    assert!(
        !h.gather_tool_definitions_for_role(&h.config.selected_role)
            .iter()
            .any(|tool| tool.name.as_str() == "staged_tool")
    );
    let system_prompt = h.build_system_prompt_for_role(&h.config.selected_role);
    assert!(!system_prompt.contains("STAGED TOOL PROMPT"));
    assert!(!system_prompt.contains("STAGED EXTENSION PROMPT"));

    append_user_message_via_event(&mut h, "s1", "before ready");
    let spid = h.send_prompt_to_agent("s1");
    let prompt = read_prompt_created(&h, &spid);
    assert!(!prompt_has_tool(&prompt, "staged_tool"));
    assert!(!prompt.system_prompt.contains("STAGED TOOL PROMPT"));
    assert!(!prompt.system_prompt.contains("STAGED EXTENSION PROMPT"));

    h.shutdown().expect("shutdown");
}

/// Installed exact subscriptions must avoid every replacement allocation, while
/// missing exact selectors preserve the legacy order and duplicate semantics.
#[test]
fn tool_started_subscription_fast_path_is_allocation_free_and_differentially_equivalent() {
    fn legacy_result(
        historical: &[EventSelector],
        live: &[EventSelector],
    ) -> (Vec<EventSelector>, Vec<EventSelector>) {
        let selector = EventSelector::Exact(tau_proto::EventName::TOOL_STARTED);
        let mut rebuilt_live = live.to_vec();
        if rebuilt_live.contains(&selector) {
            return (historical.to_vec(), rebuilt_live);
        }
        rebuilt_live.push(selector);
        (historical.to_vec(), rebuilt_live)
    }

    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cases = [
        (
            "exact-first",
            vec![EventSelector::Exact(tau_proto::EventName::TOOL_STARTED)],
            1,
            0,
            0,
        ),
        (
            "exact-after-overlap",
            vec![
                EventSelector::Prefix("tool.".to_owned()),
                EventSelector::Exact(tau_proto::EventName::TOOL_REQUEST),
                EventSelector::Exact(tau_proto::EventName::TOOL_STARTED),
                EventSelector::Exact(tau_proto::EventName::TOOL_STARTED),
            ],
            3,
            0,
            0,
        ),
        (
            "overlap-without-exact",
            vec![
                EventSelector::Prefix("tool.".to_owned()),
                EventSelector::Exact(tau_proto::EventName::TOOL_REQUEST),
                EventSelector::Exact(tau_proto::EventName::TOOL_REQUEST),
            ],
            3,
            5,
            1,
        ),
        ("empty", Vec::new(), 0, 2, 1),
    ];
    for (name, live, expected_visits, expected_clones, expected_attempts) in cases {
        let connection_id = crate::test_connection_id(name);
        connect_test_tool(&mut h, name);
        let historical = vec![
            EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
            EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
        ];
        h.runtime_io
            .bus
            .set_subscriptions(&connection_id, historical.clone(), live.clone())
            .expect("install initial selectors");
        let expected = legacy_result(&historical, &live);

        reset_tool_started_subscription_work();
        h.ensure_tool_started_subscription(&connection_id);

        assert_eq!(
            h.runtime_io.bus.historical_subscriptions(&connection_id),
            Some(expected.0.as_slice()),
            "{name}: historical selectors"
        );
        assert_eq!(
            h.runtime_io.bus.live_subscriptions(&connection_id),
            Some(expected.1.as_slice()),
            "{name}: live selectors"
        );
        let work = tool_started_subscription_work();
        assert_eq!(work.selector_visits, expected_visits, "{name}: visits");
        assert_eq!(work.selector_clones, expected_clones, "{name}: clones");
        assert_eq!(
            work.replacement_attempts, expected_attempts,
            "{name}: replacements"
        );
        if expected_attempts == 0 {
            assert_eq!(work.vector_allocations, 0, "{name}: allocations");
        }
    }
    h.shutdown().expect("shutdown");
}

/// Selector visits must scale exactly with the borrowed live slice, and only a
/// missing exact selector may clone either replacement vector.
#[test]
fn tool_started_subscription_work_scales_with_borrowed_selector_visits() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let connection_id = crate::test_connection_id("subscription-scaling");
    connect_test_tool(&mut h, connection_id.as_str());
    let historical = vec![EventSelector::Exact(tau_proto::EventName::SESSION_STARTED)];
    let mut live = (0..128)
        .map(|index| {
            EventSelector::Exact(
                format!("demo.subscription_{index}")
                    .parse()
                    .expect("custom event name"),
            )
        })
        .collect::<Vec<_>>();
    h.runtime_io
        .bus
        .set_subscriptions(&connection_id, historical.clone(), live.clone())
        .expect("install wide missing selector set");

    reset_tool_started_subscription_work();
    h.ensure_tool_started_subscription(&connection_id);
    assert_eq!(
        tool_started_subscription_work(),
        ToolStartedSubscriptionWork {
            selector_visits: 128,
            vector_allocations: 3,
            selector_clones: 129,
            replacement_attempts: 1,
        }
    );

    live.push(EventSelector::Exact(tau_proto::EventName::TOOL_STARTED));
    h.runtime_io
        .bus
        .set_subscriptions(&connection_id, historical, live)
        .expect("install wide exact selector set");
    reset_tool_started_subscription_work();
    h.ensure_tool_started_subscription(&connection_id);
    assert_eq!(
        tool_started_subscription_work(),
        ToolStartedSubscriptionWork {
            selector_visits: 129,
            ..ToolStartedSubscriptionWork::default()
        }
    );
    h.shutdown().expect("shutdown");
}

/// A rejected registration must leave the installed selectors unchanged and
/// never enter either subscription-repair path.
#[test]
fn invalid_tool_registration_does_not_repair_or_replace_subscriptions() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let connection_id = crate::test_connection_id("invalid-subscription-owner");
    connect_handshaking_tool(&mut h, connection_id.as_str());
    h.handle_extension_message(&connection_id, TestMessage::Ready(Default::default()))
        .expect("activate extension");
    let historical = vec![EventSelector::Exact(tau_proto::EventName::SESSION_STARTED)];
    let live = vec![EventSelector::Prefix("tool.".to_owned())];
    h.runtime_io
        .bus
        .set_subscriptions(&connection_id, historical.clone(), live.clone())
        .expect("install initial selectors");

    reset_tool_started_subscription_work();
    h.handle_extension_event(
        connection_id.as_str(),
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_invalid_tool_spec("invalid_subscription_tool"),
                tool_group: None,
                prompt_fragment: None,
            },
        )),
    )
    .expect("commit rejected registration");

    assert_eq!(
        tool_started_subscription_work(),
        ToolStartedSubscriptionWork::default()
    );
    assert_eq!(
        h.runtime_io.bus.historical_subscriptions(&connection_id),
        Some(historical.as_slice())
    );
    assert_eq!(
        h.runtime_io.bus.live_subscriptions(&connection_id),
        Some(live.as_slice())
    );
    assert!(
        h.tool_routing
            .registry
            .providers_for("invalid_subscription_tool")
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

#[test]
fn staged_tool_register_activates_on_ready_and_prompts_include_it() {
    // Ready is the activation boundary: the staged tool and its prompt fragment
    // become visible together before any queued prompts are advanced.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let conn_id = "conn-staged-ready";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("staged_tool"),
                tool_group: None,
                prompt_fragment: Some(tau_proto::PromptFragment::new(
                    "staged_tool.instructions",
                    tau_proto::PromptPriority::new(10),
                    "STAGED TOOL PROMPT",
                )),
            },
        )),
    )
    .expect("stage tool");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtPromptFragmentPublish(
            tau_proto::ExtPromptFragmentPublish {
                fragment: tau_proto::PromptFragment::new(
                    "staged.extension.instructions",
                    tau_proto::PromptPriority::new(20),
                    "STAGED EXTENSION PROMPT",
                ),
            },
        )),
    )
    .expect("stage extension prompt fragment");

    reset_tool_started_subscription_work();
    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert_eq!(
        tool_started_subscription_work(),
        ToolStartedSubscriptionWork {
            vector_allocations: 1,
            replacement_attempts: 1,
            ..ToolStartedSubscriptionWork::default()
        }
    );
    assert_eq!(
        h.tool_routing.registry.providers_for("staged_tool").len(),
        1
    );
    append_user_message_via_event(&mut h, "s1", "after ready");
    let spid = h.send_prompt_to_agent("s1");
    let prompt = read_prompt_created(&h, &spid);
    assert!(prompt_has_tool(&prompt, "staged_tool"));
    assert!(
        prompt
            .system_prompt
            .contains("### `staged_tool` instructions\n\nSTAGED TOOL PROMPT")
    );
    assert!(prompt.system_prompt.contains("STAGED EXTENSION PROMPT"));

    h.shutdown().expect("shutdown");
}

/// A replacement connection generation must keep its replay selectors and use
/// the allocation-free path when it already declared exact live observation.
#[test]
fn restarted_tool_generation_preserves_replay_selectors_and_exact_live_subscription() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let name = "restarted-subscription-owner";
    let connection_id = crate::test_connection_id(name);
    connect_handshaking_tool(&mut h, name);
    h.handle_extension_event(
        name,
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("first_generation_tool"),
                tool_group: None,
                prompt_fragment: None,
            },
        )),
    )
    .expect("stage first generation");
    h.handle_extension_message(&connection_id, TestMessage::Ready(Default::default()))
        .expect("activate first generation");
    h.handle_disconnect(&connection_id);

    connect_handshaking_tool(&mut h, name);
    let historical = vec![
        EventSelector::Exact(tau_proto::EventName::SESSION_STARTED),
        EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
    ];
    let live = vec![
        EventSelector::Prefix("tool.".to_owned()),
        EventSelector::Exact(tau_proto::EventName::TOOL_STARTED),
        EventSelector::Exact(tau_proto::EventName::TOOL_STARTED),
    ];
    h.runtime_io
        .bus
        .set_subscriptions(&connection_id, historical.clone(), live.clone())
        .expect("install replacement generation selectors");
    h.handle_extension_event(
        name,
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("second_generation_tool"),
                tool_group: None,
                prompt_fragment: None,
            },
        )),
    )
    .expect("stage replacement generation");

    reset_tool_started_subscription_work();
    h.handle_extension_message(&connection_id, TestMessage::Ready(Default::default()))
        .expect("activate replacement generation");

    assert_eq!(
        tool_started_subscription_work(),
        ToolStartedSubscriptionWork {
            selector_visits: 2,
            ..ToolStartedSubscriptionWork::default()
        }
    );
    assert_eq!(
        h.runtime_io.bus.historical_subscriptions(&connection_id),
        Some(historical.as_slice())
    );
    assert_eq!(
        h.runtime_io.bus.live_subscriptions(&connection_id),
        Some(live.as_slice())
    );
    assert!(
        h.tool_routing
            .registry
            .providers_for("first_generation_tool")
            .is_empty()
    );
    assert_eq!(
        h.tool_routing
            .registry
            .providers_for("second_generation_tool")
            .len(),
        1
    );
    h.shutdown().expect("shutdown");
}

/// The initial barrier activates two simultaneously configured Slack-style
/// instances only after both are ready, routes each final name to its exact
/// owner, and preserves the survivor when one disconnects.
#[test]
fn two_prefixed_instances_coexist_and_disconnect_independently() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    let mut sinks = BTreeMap::new();
    for (connection_id, prefix) in [("slack-personal", "personal"), ("slack-work", "work")] {
        let sink = connect_handshaking_tool(&mut h, connection_id);
        let prefix = tau_proto::ToolNamePrefix::parse(prefix).expect("prefix");
        let entry = h
            .extensions
            .entries
            .get_mut(connection_id)
            .expect("extension");
        entry.tool_prefix = Some(prefix.clone());
        entry.state = ExtensionState::Spawning;
        h.handle_extension_message(
            &crate::test_connection_id(connection_id),
            TestMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: crate::test_extension_name(connection_id),
                client_kind: tau_proto::ClientKind::Tool,
                expected_session_id: None,
                capabilities: Default::default(),
            }),
        )
        .expect("hello");
        let configure = sink
            .lock()
            .expect("sink")
            .iter()
            .find_map(|routed| match &routed.frame {
                HarnessOutputMessage::Configure(configure) => Some(configure.clone()),
                _ => None,
            })
            .expect("configure");
        assert_eq!(configure.tool_prefix.as_ref(), Some(&prefix));
        let mut spec = staged_tool_spec("slack_send");
        spec.model_visible_name = Some(ToolName::new("slack_send"));
        let registration = tau_client::ToolNameScope::from_configure(&configure)
            .scope_registration(tau_proto::ToolRegistrationDeclared {
                tool: spec,
                tool_group: Some(tau_proto::ToolGroup {
                    name: tau_proto::ToolGroupName::new("slack"),
                    prompt_fragment: None,
                }),
                prompt_fragment: None,
            })
            .expect("scope logical registration");
        h.handle_extension_event(
            connection_id,
            TestProtocolItem::Event(Event::ToolRegistrationDeclared(registration)),
        )
        .expect("stage tool");
        sinks.insert(connection_id, sink);
    }

    h.handle_extension_message(
        &crate::test_connection_id("slack-work"),
        TestMessage::Ready(Default::default()),
    )
    .expect("first ready");
    assert!(
        h.tool_routing
            .registry
            .providers_for("work_slack_send")
            .is_empty()
    );
    h.handle_extension_message(
        &crate::test_connection_id("slack-personal"),
        TestMessage::Ready(Default::default()),
    )
    .expect("second ready");

    assert_eq!(
        h.tool_routing.registry.providers_for("personal_slack_send")[0]
            .connection_id
            .as_str(),
        "slack-personal"
    );
    assert_eq!(
        h.tool_routing.registry.providers_for("work_slack_send")[0]
            .connection_id
            .as_str(),
        "slack-work"
    );
    for (tool_name, owner, call_id) in [
        ("personal_slack_send", "slack-personal", "personal-call"),
        ("work_slack_send", "slack-work", "work-call"),
    ] {
        let route = h
            .tool_routing
            .registry
            .route_tool_request(tau_proto::ToolRequest {
                call_id: call_id.into(),
                tool_name: ToolName::new(tool_name),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                agent_id: crate::parse_agent_id("agent"),
                originator: tau_proto::PromptOriginator::User,
            })
            .expect("route prefixed tool");
        assert_eq!(
            route.target,
            tau_core::ToolRouteTarget::Extension(crate::test_connection_id(owner))
        );
        h.runtime_io
            .bus
            .send_to(
                &crate::test_connection_id(owner),
                None,
                HarnessOutputMessage::deliver(Event::ToolStarted(route.invoke)),
            )
            .expect("deliver invoke");
        assert!(sink_has_tool_invoke(&sinks[owner], call_id));
        let other = if owner == "slack-work" {
            "slack-personal"
        } else {
            "slack-work"
        };
        assert!(!sink_has_tool_invoke(&sinks[other], call_id));
    }
    h.handle_disconnect(&crate::test_connection_id("slack-personal"));
    assert!(
        h.tool_routing
            .registry
            .providers_for("personal_slack_send")
            .is_empty()
    );
    assert_eq!(
        h.tool_routing.registry.providers_for("work_slack_send")[0]
            .connection_id
            .as_str(),
        "slack-work"
    );
    h.shutdown().expect("shutdown");
}

/// A disconnect that completes the initial terminal-state set releases already
/// received Ready stages instead of leaving them permanently withheld.
#[test]
fn optional_disconnect_completes_initial_activation_barrier() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    connect_handshaking_tool(&mut h, "ready-owner");
    connect_handshaking_tool(&mut h, "optional-blocker");
    h.extensions
        .entries
        .get_mut("optional-blocker")
        .expect("optional extension")
        .require = false;
    h.handle_extension_event(
        "ready-owner",
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("barrier_tool"),
                tool_group: None,
                prompt_fragment: None,
            },
        )),
    )
    .expect("stage tool");

    h.handle_extension_message(
        &crate::test_connection_id("ready-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("ready received");
    h.handle_extension_event(
        "ready-owner",
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("post_ready_barrier_tool"),
                tool_group: None,
                prompt_fragment: None,
            },
        )),
    )
    .expect("post-Ready declaration is deferred as runtime traffic");
    assert!(
        h.tool_routing
            .registry
            .providers_for("barrier_tool")
            .is_empty()
    );
    assert!(
        h.tool_routing
            .registry
            .providers_for("post_ready_barrier_tool")
            .is_empty()
    );
    assert_eq!(
        h.extensions.entries["ready-owner"].state,
        ExtensionState::Handshaking
    );

    h.handle_startup_disconnect(&crate::test_connection_id("optional-blocker"))
        .expect("optional disconnect degrades");

    assert_eq!(
        h.extensions.entries["ready-owner"].state,
        ExtensionState::Ready
    );
    assert_eq!(
        h.tool_routing.registry.providers_for("barrier_tool")[0]
            .connection_id
            .as_str(),
        "ready-owner"
    );
    assert_eq!(
        h.tool_routing
            .registry
            .providers_for("post_ready_barrier_tool")[0]
            .connection_id
            .as_str(),
        "ready-owner"
    );
    h.shutdown().expect("shutdown");
}

/// Initial Configure handlers may synchronously use extension-owned storage
/// before they can accept configuration and send Ready. That bootstrap RPC must
/// complete immediately, while requests sent after this peer's Ready retain
/// normal global activation ordering.
#[test]
fn pre_ready_extension_data_rpc_bypasses_only_activation_staging() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    let sink = connect_handshaking_tool(&mut h, "config-rpc");
    connect_handshaking_tool(&mut h, "activation-blocker");
    let request = |request_id: &str, op| {
        HarnessInputMessage::ExtensionDataRequest(tau_proto::ExtensionDataRequest {
            request_id: request_id.to_owned(),
            scope: tau_proto::ExtensionDataScope::User,
            expected_session_id: None,
            op,
        })
    };
    let has_result = |request_id: &str| {
        sink.lock().expect("sink").iter().any(|routed| {
            matches!(
                &routed.frame,
                HarnessOutputMessage::ExtensionDataResult(result)
                    if result.request_id == request_id
            )
        })
    };

    h.handle_extension_message(
        &crate::test_connection_id("config-rpc"),
        request(
            "configure-write",
            tau_proto::ExtensionDataRequestOp::WriteFile {
                path: tau_proto::ExtensionDataPath::from("state.json"),
                contents: b"configured".to_vec(),
            },
        ),
    )
    .expect("pre-Ready config storage request");
    assert!(has_result("configure-write"));

    h.handle_extension_message(
        &crate::test_connection_id("config-rpc"),
        TestMessage::Ready(Default::default()),
    )
    .expect("peer Ready waits on blocker");
    h.handle_extension_message(
        &crate::test_connection_id("config-rpc"),
        request(
            "post-ready-read",
            tau_proto::ExtensionDataRequestOp::ReadFile {
                path: tau_proto::ExtensionDataPath::from("state.json"),
            },
        ),
    )
    .expect("post-Ready request is deferred");
    assert!(!has_result("post-ready-read"));

    h.handle_extension_message(
        &crate::test_connection_id("activation-blocker"),
        TestMessage::Ready(Default::default()),
    )
    .expect("release activation barrier");
    assert!(has_result("post-ready-read"));
    h.shutdown().expect("shutdown");
}

/// `Ready` freezes each peer's startup claims. A declaration received after it
/// has sent `Ready` uses runtime collision handling whether another peer is
/// still blocking initial activation or the barrier has already completed.
#[test]
fn post_ready_tool_registration_has_topology_independent_runtime_semantics() {
    let run = |blocked_at_registration: bool| {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        h.extensions.initial_tool_preflight_complete = false;
        connect_handshaking_tool(&mut h, "late-claimant");
        connect_handshaking_tool(&mut h, "startup-owner");
        h.extensions
            .entries
            .get_mut("late-claimant")
            .expect("claimant")
            .require = false;
        h.handle_extension_event(
            "startup-owner",
            TestProtocolItem::Event(Event::ToolRegistrationDeclared(
                tau_proto::ToolRegistrationDeclared {
                    tool: staged_tool_spec("ready_frozen_tool"),
                    tool_group: None,
                    prompt_fragment: None,
                },
            )),
        )
        .expect("stage startup owner");
        h.handle_extension_message(
            &crate::test_connection_id("late-claimant"),
            TestMessage::Ready(Default::default()),
        )
        .expect("claimant Ready");
        if !blocked_at_registration {
            h.handle_extension_message(
                &crate::test_connection_id("startup-owner"),
                TestMessage::Ready(Default::default()),
            )
            .expect("complete initial barrier");
        }
        h.handle_extension_event(
            "late-claimant",
            TestProtocolItem::Event(Event::ToolRegistrationDeclared(
                tau_proto::ToolRegistrationDeclared {
                    tool: staged_tool_spec("ready_frozen_tool"),
                    tool_group: None,
                    prompt_fragment: None,
                },
            )),
        )
        .expect("late runtime claim is isolated");
        if blocked_at_registration {
            h.handle_extension_message(
                &crate::test_connection_id("startup-owner"),
                TestMessage::Ready(Default::default()),
            )
            .expect("release initial barrier");
        }
        assert_eq!(
            h.tool_routing.registry.providers_for("ready_frozen_tool")[0]
                .connection_id
                .as_str(),
            "startup-owner"
        );
        assert_eq!(
            h.extensions.entries["late-claimant"].state,
            ExtensionState::Ready
        );
        assert_eq!(
            h.tool_routing
                .registry
                .providers_for("ready_frozen_tool")
                .len(),
            1
        );
        h.shutdown().expect("shutdown");
    };

    run(true);
    run(false);
}

/// The compatibility path for a required non-provider tool disconnect also
/// completes the barrier for peers that already sent Ready.
#[test]
fn required_tool_disconnect_completes_initial_activation_barrier() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    connect_handshaking_tool(&mut h, "ready-owner");
    connect_handshaking_tool(&mut h, "required-tool-blocker");
    h.handle_extension_event(
        "ready-owner",
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("required_disconnect_barrier_tool"),
                tool_group: None,
                prompt_fragment: None,
            },
        )),
    )
    .expect("stage tool");
    h.handle_extension_message(
        &crate::test_connection_id("ready-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("ready received");

    h.handle_startup_disconnect(&crate::test_connection_id("required-tool-blocker"))
        .expect("required tool compatibility disconnect");

    assert_eq!(
        h.extensions.entries["ready-owner"].state,
        ExtensionState::Ready
    );
    assert_eq!(
        h.tool_routing
            .registry
            .providers_for("required_disconnect_barrier_tool")[0]
            .connection_id
            .as_str(),
        "ready-owner"
    );
    h.shutdown().expect("shutdown");
}

/// Timeout classification excludes peers that already sent Ready, disables only
/// the actual optional blocker, and then activates the ready peer.
#[test]
fn optional_timeout_completes_barrier_for_required_ready_peer() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    connect_handshaking_tool(&mut h, "required-ready");
    connect_handshaking_tool(&mut h, "optional-timeout");
    h.extensions
        .entries
        .get_mut("optional-timeout")
        .expect("optional")
        .require = false;
    h.handle_extension_event(
        "required-ready",
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("timeout_barrier_tool"),
                tool_group: None,
                prompt_fragment: None,
            },
        )),
    )
    .expect("stage tool");
    h.handle_extension_message(
        &crate::test_connection_id("required-ready"),
        TestMessage::Ready(Default::default()),
    )
    .expect("ready received");

    h.handle_extensions_startup_timeout()
        .expect("optional timeout degrades");

    assert_eq!(
        h.extensions.entries["required-ready"].state,
        ExtensionState::Ready
    );
    assert_eq!(
        h.extensions.entries["optional-timeout"].state,
        ExtensionState::Disconnected
    );
    assert_eq!(
        h.tool_routing
            .registry
            .providers_for("timeout_barrier_tool")[0]
            .connection_id
            .as_str(),
        "required-ready"
    );
    h.shutdown().expect("shutdown");
}

/// An empty configured extension set still closes the one-time initial barrier.
#[test]
fn empty_extension_set_completes_initial_activation_barrier() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.extensions.entries.clear();
    h.extensions.order.clear();
    h.extensions.activation_staging.clear();
    h.extensions.ready_received.clear();
    h.extensions.pending_connects = 0;
    h.extensions.initial_tool_preflight_complete = false;

    h.wait_for_extensions_ready().expect("empty barrier");

    assert!(h.extensions.initial_tool_preflight_complete);
    h.shutdown().expect("shutdown");
}

/// A malformed frame from an already-live extension isolates that peer rather
/// than terminating the harness event loop.
#[test]
fn runtime_duplicate_ready_disconnects_only_the_extension() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    connect_handshaking_tool(&mut h, "runtime-bad");
    h.extensions
        .entries
        .get_mut("runtime-bad")
        .expect("extension")
        .state = ExtensionState::Ready;
    h.extensions.initial_tool_preflight_complete = true;

    h.handle_extension_message(
        &crate::test_connection_id("runtime-bad"),
        TestMessage::Ready(Default::default()),
    )
    .expect("runtime protocol failure is isolated");

    assert_eq!(
        h.extensions.entries["runtime-bad"].state,
        ExtensionState::Disconnected
    );
    h.shutdown().expect("shutdown");
}

/// Config rejection after the initial barrier isolates the live connection
/// without applying optional-startup permanent-disable policy.
#[test]
fn runtime_config_error_disconnects_without_disabling_respawn_policy() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    connect_handshaking_tool(&mut h, "runtime-config-bad");
    let entry = h
        .extensions
        .entries
        .get_mut("runtime-config-bad")
        .expect("extension");
    entry.state = ExtensionState::Ready;
    entry.require = false;
    entry.respawn_allowed = true;
    h.extensions.initial_tool_preflight_complete = true;

    h.handle_extension_message(
        &crate::test_connection_id("runtime-config-bad"),
        TestMessage::ConfigError(tau_proto::ConfigError {
            message: "runtime update rejected".to_owned(),
        }),
    )
    .expect("runtime config failure isolated");

    let entry = &h.extensions.entries["runtime-config-bad"];
    assert_eq!(entry.state, ExtensionState::Disconnected);
    assert!(entry.respawn_allowed);
    h.shutdown().expect("shutdown");
}

/// Internal tool reservations are present before initial extension preflight:
/// optional conflicts degrade and required conflicts fail startup.
#[test]
fn initial_internal_tool_conflicts_follow_availability_policy() {
    let run = |required: bool| {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        h.extensions.initial_tool_preflight_complete = false;
        h.tool_routing.registry.register_internal(
            &crate::test_connection_id("harness"),
            staged_tool_spec("reserved_tool"),
        );
        connect_handshaking_tool(&mut h, "claimant");
        h.extensions
            .entries
            .get_mut("claimant")
            .expect("claimant")
            .require = required;
        h.handle_extension_event(
            "claimant",
            TestProtocolItem::Event(Event::ToolRegistrationDeclared(
                tau_proto::ToolRegistrationDeclared {
                    tool: staged_tool_spec("reserved_tool"),
                    tool_group: None,
                    prompt_fragment: None,
                },
            )),
        )
        .expect("stage conflicting tool");
        let result = h.handle_extension_message(
            &crate::test_connection_id("claimant"),
            TestMessage::Ready(Default::default()),
        );
        (h, result)
    };

    let (mut optional, result) = run(false);
    result.expect("optional conflict degrades");
    assert_eq!(
        optional.extensions.entries["claimant"].state,
        ExtensionState::Disconnected
    );
    assert_eq!(
        optional
            .tool_routing
            .registry
            .providers_for("reserved_tool")[0]
            .kind,
        tau_core::ToolProviderKind::Internal
    );
    optional.shutdown().expect("shutdown");

    let (mut required, result) = run(true);
    assert!(result.is_err(), "required internal conflict is fatal");
    assert_eq!(
        required
            .tool_routing
            .registry
            .providers_for("reserved_tool")[0]
            .kind,
        tau_core::ToolProviderKind::Internal
    );
    required.shutdown().expect("shutdown");
}

/// Initial collision outcomes follow required/optional policy rather than Ready
/// arrival order.
#[test]
fn initial_tool_collision_matrix_is_deterministic() {
    let run = |requirements: &[(&str, bool)], reverse_ready: bool| {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        h.extensions.initial_tool_preflight_complete = false;
        for (connection_id, required) in requirements {
            connect_handshaking_tool(&mut h, connection_id);
            h.extensions
                .entries
                .get_mut(*connection_id)
                .expect("extension")
                .require = *required;
            h.handle_extension_event(
                connection_id,
                TestProtocolItem::Event(Event::ToolRegistrationDeclared(
                    tau_proto::ToolRegistrationDeclared {
                        tool: staged_tool_spec("shared_startup_tool"),
                        tool_group: None,
                        prompt_fragment: None,
                    },
                )),
            )
            .expect("stage tool");
        }
        let mut result = Ok(());
        let mut ready_order = requirements.iter().collect::<Vec<_>>();
        if reverse_ready {
            ready_order.reverse();
        }
        for (connection_id, _) in ready_order {
            result = h.handle_extension_message(
                &crate::test_connection_id(*connection_id),
                TestMessage::Ready(Default::default()),
            );
            if result.is_err() {
                break;
            }
        }
        (h, result)
    };

    for reverse_ready in [false, true] {
        let (mut required_optional, result) = run(
            &[("optional-owner", false), ("required-owner", true)],
            reverse_ready,
        );
        result.expect("required wins");
        assert_eq!(
            required_optional
                .tool_routing
                .registry
                .providers_for("shared_startup_tool")[0]
                .connection_id
                .as_str(),
            "required-owner"
        );
        assert_eq!(
            required_optional.extensions.entries["optional-owner"].state,
            ExtensionState::Disconnected
        );
        required_optional.shutdown().expect("shutdown");

        let (mut optional_optional, result) = run(
            &[("optional-a", false), ("optional-b", false)],
            reverse_ready,
        );
        result.expect("optional conflicts degrade");
        assert!(
            optional_optional
                .tool_routing
                .registry
                .providers_for("shared_startup_tool")
                .is_empty()
        );
        optional_optional.shutdown().expect("shutdown");

        let (mut required_required, result) =
            run(&[("required-a", true), ("required-b", true)], reverse_ready);
        assert!(result.is_err(), "required collision must fail startup");
        assert!(
            required_required
                .tool_routing
                .registry
                .providers_for("shared_startup_tool")
                .is_empty()
        );
        required_required.shutdown().expect("shutdown");
    }
}

/// Invalid staged registrations are removed before name ownership is computed:
/// they cannot make a valid peer lose a startup collision.
#[test]
fn invalid_initial_tool_registration_does_not_claim_collision_ownership() {
    let run = |invalid_required: bool| {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        h.extensions.initial_tool_preflight_complete = false;
        for (connection_id, required, spec) in [
            (
                "invalid-owner",
                invalid_required,
                staged_invalid_tool_spec("invalid_collision_tool"),
            ),
            (
                "valid-owner",
                false,
                staged_tool_spec("invalid_collision_tool"),
            ),
        ] {
            connect_handshaking_tool(&mut h, connection_id);
            h.extensions
                .entries
                .get_mut(connection_id)
                .expect("extension")
                .require = required;
            h.handle_extension_event(
                connection_id,
                TestProtocolItem::Event(Event::ToolRegistrationDeclared(
                    tau_proto::ToolRegistrationDeclared {
                        tool: spec,
                        tool_group: None,
                        prompt_fragment: None,
                    },
                )),
            )
            .expect("stage registration");
        }
        h.handle_extension_message(
            &crate::test_connection_id("invalid-owner"),
            TestMessage::Ready(Default::default()),
        )
        .expect("first Ready waits");
        let result = h.handle_extension_message(
            &crate::test_connection_id("valid-owner"),
            TestMessage::Ready(Default::default()),
        );
        (h, result)
    };

    let (mut optional_invalid, result) = run(false);
    result.expect("invalid optional peer degrades");
    assert_eq!(
        optional_invalid
            .tool_routing
            .registry
            .providers_for("invalid_collision_tool")[0]
            .connection_id
            .as_str(),
        "valid-owner"
    );
    assert_eq!(
        optional_invalid.extensions.entries["invalid-owner"].state,
        ExtensionState::Disconnected
    );
    optional_invalid.shutdown().expect("shutdown");

    let (mut required_invalid, result) = run(true);
    assert!(result.is_err(), "invalid required peer is fatal");
    assert!(
        required_invalid
            .tool_routing
            .registry
            .providers_for("invalid_collision_tool")
            .is_empty()
    );
    required_invalid.shutdown().expect("shutdown");
}

/// Initial preflight validates only the final same-owner registration for a
/// name, preserving last-refresh semantics in both validity directions.
#[test]
fn initial_tool_refresh_validates_only_last_same_owner_registration() {
    let run = |first: ToolSpec, second: ToolSpec| {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        h.extensions.initial_tool_preflight_complete = false;
        connect_handshaking_tool(&mut h, "refresh-owner");
        for tool in [first, second] {
            h.handle_extension_event(
                "refresh-owner",
                TestProtocolItem::Event(Event::ToolRegistrationDeclared(
                    tau_proto::ToolRegistrationDeclared {
                        tool,
                        tool_group: None,
                        prompt_fragment: None,
                    },
                )),
            )
            .expect("stage refresh");
        }
        let result = h.handle_extension_message(
            &crate::test_connection_id("refresh-owner"),
            TestMessage::Ready(Default::default()),
        );
        (h, result)
    };

    let (mut valid_final, result) = run(
        staged_invalid_tool_spec("refreshed_tool"),
        staged_tool_spec("refreshed_tool"),
    );
    result.expect("valid final refresh wins");
    assert_eq!(
        valid_final
            .tool_routing
            .registry
            .providers_for("refreshed_tool")
            .len(),
        1
    );
    valid_final.shutdown().expect("shutdown");

    let (mut invalid_final, result) = run(
        staged_tool_spec("refreshed_tool"),
        staged_invalid_tool_spec("refreshed_tool"),
    );
    assert!(result.is_err(), "invalid final refresh remains fatal");
    assert!(
        invalid_final
            .tool_routing
            .registry
            .providers_for("refreshed_tool")
            .is_empty()
    );
    invalid_final.shutdown().expect("shutdown");
}

#[test]
fn tool_prompt_fragment_heading_uses_model_visible_tool_name() {
    // Tool prompt fragments are grouped by the tool name the model can call, so
    // the automatic heading must use the same model-visible alias as the tool
    // definition instead of the provider's internal routing name.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let conn_id = "conn-staged-visible-tool";
    let _sink = connect_handshaking_tool(&mut h, conn_id);
    let mut spec = staged_tool_spec("internal_staged_tool");
    spec.model_visible_name = Some(ToolName::new("visible_staged_tool"));

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: spec,
                tool_group: None,
                prompt_fragment: Some(tau_proto::PromptFragment::new(
                    "visible_staged_tool.instructions",
                    tau_proto::PromptPriority::new(10),
                    "ALIASED TOOL PROMPT",
                )),
            },
        )),
    )
    .expect("stage tool");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("empty_fragment_tool"),
                tool_group: None,
                prompt_fragment: Some(tau_proto::PromptFragment::new(
                    "empty_fragment_tool.instructions",
                    tau_proto::PromptPriority::new(10),
                    "",
                )),
            },
        )),
    )
    .expect("stage empty prompt tool");
    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    append_user_message_via_event(&mut h, "s1", "after ready");
    let spid = h.send_prompt_to_agent("s1");
    let prompt = read_prompt_created(&h, &spid);

    assert!(
        prompt
            .system_prompt
            .contains("### `visible_staged_tool` instructions\n\nALIASED TOOL PROMPT")
    );
    assert!(
        !prompt
            .system_prompt
            .contains("### `internal_staged_tool` instructions")
    );

    assert!(
        !prompt
            .system_prompt
            .contains("### `empty_fragment_tool` instructions")
    );

    h.shutdown().expect("shutdown");
}

/// A prompt-owned queued call must retain its payload and provider position
/// while an exact replacement registration waits behind the Ready barrier.
#[test]
fn queued_tool_call_waits_for_staged_provider_until_ready() {
    // Regression: prompt-owned calls must use the prompt's advertised tool
    // snapshot, not current role policy. A tool can sit behind another
    // in-flight call after the live provider disappears, current policy changes
    // to disallow the tool, and a replacement provider is still handshaking.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let blocking_sink = connect_ready_configured_extension(
        &mut h,
        "conn-blocking-tool",
        "configured-blocking-tool",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-blocking-tool"),
        staged_tool_spec("blocking_tool"),
    );
    let old_provider = connect_test_tool(&mut h, "conn-old-staged-tool");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-old-staged-tool"),
        staged_tool_spec("staged_tool"),
    );

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-staged-tools");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-staged-tools"), cid.clone());
    assert!(
        h.prompt_coordination.prompt_runtime.tool_specs[&test_agent_prompt_id("sp-staged-tools")]
            .iter()
            .any(|spec| spec.name == "staged_tool")
    );

    h.tool_routing
        .registry
        .unregister_connection(&crate::test_connection_id("conn-old-staged-tool"));
    drop(old_provider);
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role")
        .disable_tools
        .push(ToolName::new("staged_tool"));

    let staged_sink = connect_handshaking_tool(&mut h, "conn-staged-tool");
    h.handle_extension_event(
        "conn-staged-tool",
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("staged_tool"),
                tool_group: None,
                prompt_fragment: None,
            },
        )),
    )
    .expect("stage tool");
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "run two tools".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    crate::tool_turn::start_pending_tool_ownership_probe("call-staged");

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-staged-tools"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-blocking".into(),
                name: ToolName::new("blocking_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-staged".into(),
                name: ToolName::new("staged_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Text("x".repeat(1024 * 1024)),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    assert!(sink_has_tool_invoke(&blocking_sink, "call-blocking"));
    assert!(!sink_has_tool_invoke(&staged_sink, "call-staged"));
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 1);

    h.handle_extension_event(
        "conn-blocking-tool",
        TestProtocolItem::Event(test_tool_result("call-blocking", "blocking_tool")),
    )
    .expect("blocking result");

    assert!(!sink_has_tool_invoke(&staged_sink, "call-staged"));
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 1);
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.in_flight_len(), 0);

    h.handle_extension_message(
        &crate::test_connection_id("conn-staged-tool"),
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(sink_has_tool_invoke(&staged_sink, "call-staged"));
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .get("call-staged")
            .map(|provider| provider.as_str()),
        Some("conn-staged-tool")
    );
    let ownership = crate::tool_turn::finish_pending_tool_ownership_probe("call-staged");
    assert_eq!(ownership.pending_clones, 0);
    assert_eq!(ownership.candidate_visits, 3);
    assert_eq!(ownership.queue_pops, 1);
    assert_ne!(ownership.admission_text_ptr, 0);
    assert_eq!(ownership.admission_text_ptr, ownership.popped_text_ptr);
    assert_eq!(ownership.popped_text_ptr, ownership.execution_text_ptr);

    h.handle_extension_event(
        "conn-staged-tool",
        TestProtocolItem::Event(test_tool_result("call-staged", "staged_tool")),
    )
    .expect("staged result");
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("call-staged")
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn extension_that_never_sends_ready_never_exposes_staged_tool() {
    // A handshaking extension may never finish. Its staged tools must remain
    // unavailable and prompt dispatch stays queued behind the existing Ready
    // gate instead of leaking half-initialized capabilities.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let conn_id = "conn-never-ready";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("never_ready_tool"),
                tool_group: None,
                prompt_fragment: None,
            },
        )),
    )
    .expect("stage tool");

    let submission = h
        .submit_user_prompt(test_session_id("s1"), "try never ready tool".to_owned())
        .expect("submit");
    assert!(matches!(submission, PromptSubmission::Queued));
    assert!(
        h.tool_routing
            .registry
            .providers_for("never_ready_tool")
            .is_empty()
    );
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_models_are_staged_until_ready_and_queued_prompt_waits() {
    // Provider model snapshots define both visible model state and prompt
    // routing. A handshaking provider must not make a queued prompt dispatch
    // until its Ready message activates the staged snapshot.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    clear_quiet_provider_models(&mut h);
    assert!(h.config.selected_model.is_none());

    let conn_id = "conn-staged-provider";
    let _sink = connect_handshaking_extension(&mut h, conn_id, tau_proto::ClientKind::Provider);
    let model_name = "staged/provider-model";
    let model_id: tau_proto::ModelId = model_name.into();
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ProviderModelsDeclared(
            tau_proto::ProviderModelsDeclared {
                models: vec![staged_provider_model(model_name)],
            },
        )),
    )
    .expect("stage provider models");

    let submission = h
        .submit_user_prompt(test_session_id("s1"), "wait for staged model".to_owned())
        .expect("submit");
    assert!(matches!(submission, PromptSubmission::Queued));
    assert!(!h.provider_runtime.available_models.contains(&model_id));
    assert!(!h.provider_runtime.model_routes.contains_key(&model_id));
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );
    assert!(event_log_contains_source_event(&h, conn_id, |event| {
        matches!(event, Event::ProviderModelsDeclared(_))
    }));
    assert!(!event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| {
            matches!(
                event,
                Event::ProviderModelsUpdated(update)
                    if update.models.iter().any(|model| model.id == model_id)
            )
        }
    ));

    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(h.provider_runtime.available_models.contains(&model_id));
    assert_eq!(
        h.provider_runtime
            .model_routes
            .get(&model_id)
            .map(|id| id.as_str()),
        Some(conn_id)
    );
    assert!(event_log_contains_source_event(&h, conn_id, |event| {
        matches!(event, Event::ProviderModelsDeclared(update) if update.models.iter().any(|model| model.id == model_id))
    }));
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| {
            matches!(event, Event::ProviderModelsUpdated(update) if update.models.iter().any(|model| model.id == model_id))
        }
    ));
    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(prompt.model, model_id);
    assert!(prompt_context_contains(&prompt, "wait for staged model"));

    h.shutdown().expect("shutdown");
}

/// `Ready` cannot overtake pre-Ready declarations that are still parked in
/// generic interception; activation observes their final committed replacement.
#[test]
fn provider_ready_waits_for_intercepted_declarations_and_coalesces_final_state() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    clear_quiet_provider_models(&mut h);
    let conn_id = "intercepted-startup-provider";
    connect_handshaking_extension(&mut h, conn_id, tau_proto::ClientKind::Provider);
    connect_test_tool(&mut h, "startup-model-interceptor");
    h.handle_extension_event(
        "startup-model-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_MODELS_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let model: tau_proto::ModelId = "staged/intercepted".into();

    for models in [
        vec![staged_provider_model("staged/intercepted")],
        Vec::<tau_proto::ProviderModelInfo>::new(),
    ] {
        h.handle_extension_event(
            conn_id,
            TestProtocolItem::Event(Event::ProviderModelsDeclared(
                tau_proto::ProviderModelsDeclared { models },
            )),
        )
        .expect("admit declaration");
    }
    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("receive ready");
    assert_eq!(
        h.extensions.entries[conn_id].state,
        ExtensionState::Handshaking
    );
    assert!(h.extensions.ready_received.contains(conn_id));
    assert!(!h.provider_runtime.model_routes.contains_key(&model));

    h.handle_extension_event(
        "startup-model-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit first declaration");
    assert_eq!(
        h.extensions.entries[conn_id].state,
        ExtensionState::Handshaking
    );
    assert!(matches!(
        h.runtime_io.publication.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::ProviderModelsDeclared(update)) if update.models.is_empty()
    ));

    h.handle_extension_event(
        "startup-model-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit final declaration");
    assert_eq!(h.extensions.entries[conn_id].state, ExtensionState::Ready);
    assert!(!h.provider_runtime.model_routes.contains_key(&model));
    assert_eq!(
        h.provider_runtime
            .models_by_extension
            .get(conn_id)
            .expect("active empty provider snapshot"),
        &Vec::<tau_proto::ProviderModelInfo>::new()
    );
    let canonical = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::ProviderModelsUpdated(update)
                if update.publisher_extension_id.as_str() == conn_id =>
            {
                Some(update)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(canonical.len(), 2);
    assert!(canonical[0].models.iter().any(|info| info.id == model));
    assert!(canonical[1].models.is_empty());
}

/// A required initial provider's oversized interception replacement propagates
/// the startup-fatal activation quota error.
#[test]
fn required_intercepted_provider_replacement_overflow_fails_startup() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    h.extensions.initial_tool_preflight_complete = false;
    let conn_id = "oversized-replacement-provider";
    connect_handshaking_extension(&mut h, conn_id, tau_proto::ClientKind::Provider);
    connect_test_tool(&mut h, "replacement-size-interceptor");
    h.handle_extension_event(
        "replacement-size-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_MODELS_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ProviderModelsDeclared(
            tau_proto::ProviderModelsDeclared {
                models: vec![staged_provider_model("staged/small")],
            },
        )),
    )
    .expect("admit small declaration");

    let mut oversized = staged_provider_model("staged/oversized");
    oversized.display_name = Some("x".repeat(MAX_EXTENSION_ACTIVATION_BYTES));
    let error = h
        .handle_extension_event(
            "replacement-size-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(Event::ProviderModelsDeclared(
                    tau_proto::ProviderModelsDeclared {
                        models: vec![oversized],
                    },
                )))),
            })),
        )
        .expect_err("required provider overflow must fail startup");

    assert!(error.to_string().contains("activation staging exceeds"));
    assert_eq!(
        h.extensions.entries[conn_id].state,
        ExtensionState::Handshaking
    );
    assert!(
        !h.provider_runtime
            .model_routes
            .contains_key(&tau_proto::ModelId::from("staged/oversized"))
    );
}

/// Resolving a provider declaration as the last initial-barrier blocker must
/// propagate an unrelated required tool collision instead of logging and
/// partially activating the staged extensions.
#[test]
fn intercepted_provider_resolution_propagates_initial_tool_collision() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    h.extensions.initial_tool_preflight_complete = false;
    for owner in ["required-a", "required-b"] {
        connect_handshaking_tool(&mut h, owner);
        h.handle_extension_event(
            owner,
            TestProtocolItem::Event(Event::ToolRegistrationDeclared(
                tau_proto::ToolRegistrationDeclared {
                    tool: staged_tool_spec("intercept_blocked_collision"),
                    tool_group: None,
                    prompt_fragment: None,
                },
            )),
        )
        .expect("stage colliding tool");
    }
    let provider = "collision-barrier-provider";
    connect_handshaking_extension(&mut h, provider, tau_proto::ClientKind::Provider);
    connect_test_tool(&mut h, "collision-barrier-interceptor");
    h.handle_extension_event(
        "collision-barrier-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_MODELS_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event(
        provider,
        TestProtocolItem::Event(Event::ProviderModelsDeclared(
            tau_proto::ProviderModelsDeclared {
                models: vec![staged_provider_model("staged/collision-barrier")],
            },
        )),
    )
    .expect("park provider declaration");
    for source in ["required-a", "required-b", provider] {
        h.handle_extension_message(
            &crate::test_connection_id(source),
            TestMessage::Ready(Default::default()),
        )
        .expect("ready waits on provider declaration");
    }

    let error = h
        .handle_extension_event(
            "collision-barrier-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect_err("required collision must fail startup");
    assert!(error.to_string().contains("required extensions"));
    assert!(
        h.tool_routing
            .registry
            .providers_for("intercept_blocked_collision")
            .is_empty()
    );
    assert_eq!(
        h.extensions.entries[provider].state,
        ExtensionState::Handshaking
    );
}

#[test]
fn startup_session_dir_is_reported_before_extension_ready() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let events = event_log_events(&h);
    let session_dir = events
        .iter()
        .position(|event| matches!(event, Event::HarnessSessionDir(_)))
        .expect("session dir event");
    let extension_ready = events
        .iter()
        .position(|event| matches!(event, Event::ExtensionReady(_)))
        .expect("extension ready event");

    assert!(session_dir < extension_ready);

    h.shutdown().expect("shutdown");
}

/// Ensures session-wide context acknowledgements remain observable first-party
/// protocol events, preventing the harness from treating them only as private
/// wait-state signals.
#[test]
fn session_context_ready_is_published_live() {
    // Session-wide context acknowledgements are first-party protocol events, not
    // just private wait-state signals. Subscribers must observe them when an
    // extension finishes session skill/AGENTS.md refresh.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "conn-session-context-ready";
    let _extension_sink = connect_handshaking_tool(&mut h, conn_id);
    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtensionSessionContextProviderRegister(
            tau_proto::ExtensionSessionContextProviderRegister {},
        )),
    )
    .expect("register session context provider");

    let observer = connect_test_client(
        &mut h,
        "session-context-ready-observer",
        tau_proto::ClientKind::Ui,
    );
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("session-context-ready-observer"),
            Vec::new(),
            vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::EXTENSION_SESSION_CONTEXT_READY,
            )],
        )
        .expect("subscribe to session context ready");

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtensionSessionContextReady(
            tau_proto::ExtensionSessionContextReady {
                session_id: test_session_id("s1"),
            },
        )),
    )
    .expect("session context ready");

    assert!(event_log_contains_source_event(&h, conn_id, |event| {
        matches!(event, Event::ExtensionSessionContextReady(ready) if ready.session_id == "s1")
    }));
    assert!(observer.lock().expect("observer").iter().any(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::ExtensionSessionContextReady(ready)) if ready.session_id == "s1"
        )
    }));

    h.shutdown().expect("shutdown");
}

/// Interceptor registration must remain inactive before Ready, then immediately
/// receive later prompt-draft observations after the extension becomes ready.
#[test]
fn interceptor_registration_is_staged_until_ready() {
    // Interception is an extension capability: before Ready, matching events
    // must pass through normally; after Ready, the same selector becomes active.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "conn-staged-interceptor";
    let sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        }),
    )
    .expect("stage intercept");
    h.publish_event(None, draft_event("before ready"));
    assert!(
        sink.lock()
            .expect("sink")
            .iter()
            .all(|routed| { !matches!(routed.frame, HarnessOutputMessage::InterceptRequest(_)) })
    );

    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");
    h.publish_event(None, draft_event("after ready"));

    assert!(sink.lock().expect("sink").iter().any(|routed| {
        matches!(&routed.frame, HarnessOutputMessage::InterceptRequest(req)
            if matches!(req.event.as_ref(), Event::UiPromptDraft(draft) if draft.text.as_deref() == Some("after ready")))
    }));

    h.shutdown().expect("shutdown");
}

/// Pre-Ready operational emits from multiple extensions remain globally
/// deferred and commit in wire order before a start-agent request creates work.
#[test]
fn extension_emit_and_start_agent_request_are_deferred_in_order_until_ready() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    let first_id = "ordering-first";
    let second_id = "ordering-second";
    let _first_sink = connect_handshaking_tool(&mut h, first_id);
    let _second_sink = connect_handshaking_tool(&mut h, second_id);
    let custom_name: tau_proto::EventName = "demo.startup_state".parse().expect("event name");
    let trailing_name: tau_proto::EventName = "demo.after_query".parse().expect("event name");

    h.handle_extension_message(
        &crate::test_connection_id(first_id),
        TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ExtensionEvent(
                tau_proto::CustomEvent::try_new(
                    custom_name.clone(),
                    Some(test_session_id("s1")),
                    CborValue::Text("STAGED CUSTOM EVENT".to_owned()),
                )
                .expect("valid custom event"),
            )),
            persist: true,
        }),
    )
    .expect("stage emit");
    h.handle_extension_event(
        second_id,
        TestProtocolItem::Event(Event::StartAgentRequest(StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-staged".to_owned(),
            instruction: "STAGED START AGENT REQUEST".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        })),
    )
    .expect("stage query");
    h.handle_extension_event(
        first_id,
        TestProtocolItem::Event(Event::ExtensionEvent(
            tau_proto::CustomEvent::try_new(
                trailing_name.clone(),
                Some(test_session_id("s1")),
                CborValue::Text("AFTER START REQUEST".to_owned()),
            )
            .expect("valid custom event"),
        )),
    )
    .expect("stage trailing event");

    assert!(!event_log_contains_source_event(&h, first_id, |event| {
        event.name() == custom_name
    }));
    assert!(
        !h.agent_runtime
            .agent_registry
            .agents
            .keys()
            .any(|cid| cid.as_str().contains("q-staged"))
    );
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );

    h.handle_extension_message(
        &crate::test_connection_id(second_id),
        TestMessage::Ready(tau_proto::Ready {
            message: Some("second ready first".to_owned()),
        }),
    )
    .expect("second ready");
    assert!(!event_log_contains_source_event(&h, second_id, |event| {
        matches!(event, Event::StartAgentRequest(_))
    }));
    h.handle_extension_message(
        &crate::test_connection_id(first_id),
        TestMessage::Ready(tau_proto::Ready {
            message: Some("first ready second".to_owned()),
        }),
    )
    .expect("first ready");

    assert!(event_log_contains_source_event(&h, first_id, |event| {
        event.name() == custom_name
    }));
    let committed: Vec<_> = {
        let mut events = Vec::new();
        let mut seq = path_crate_event_log::EventLogSeq::new(0);
        while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
            seq = entry.seq.next();
            let relevant = entry.event.name() == custom_name
                || entry.event.name() == tau_proto::EventName::AGENT_START_REQUEST
                || entry.event.name() == trailing_name;
            if relevant {
                events.push((entry.source, entry.event.name()));
            }
        }
        events
    };
    assert_eq!(
        committed,
        [
            (Some(crate::test_connection_id(first_id)), custom_name),
            (
                Some(crate::test_connection_id(second_id)),
                tau_proto::EventName::AGENT_START_REQUEST
            ),
            (Some(crate::test_connection_id(first_id)), trailing_name)
        ]
    );
    assert!(h.agent_runtime.agent_registry.agents.iter().any(|(cid, conv)| {
        conv.identity.agent_id.as_deref() == Some(cid.as_str())
            && matches!(
                &conv.identity.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. } if query_id == "q-staged"
            )
    }));
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt_context_contains(prompt, "STAGED START AGENT REQUEST")
    )));

    h.shutdown().expect("shutdown");
}

/// Pre-Ready terminal-output events are operational traffic that commits in
/// original wire order only after the configured extension activates.
#[test]
fn terminal_output_events_are_deferred_in_order_until_ready() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "terminal-output-owner";
    connect_handshaking_tool(&mut h, conn_id);
    let observer = connect_test_client(
        &mut h,
        "terminal-output-observer",
        tau_proto::ClientKind::Ui,
    );
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("terminal-output-observer"),
            Vec::new(),
            vec![
                EventSelector::Exact(tau_proto::EventName::TERM_BELL),
                EventSelector::Exact(tau_proto::EventName::TERM_OSC1337_SET_USER_VAR),
            ],
        )
        .expect("subscribe to terminal output");

    for (event, persist) in [
        (Event::TermBell(tau_proto::TermBell {}), true),
        (
            Event::Osc1337SetUserVar(tau_proto::Osc1337SetUserVar {
                name: "status".to_owned(),
                value: "ready".to_owned(),
            }),
            false,
        ),
    ] {
        h.handle_extension_message(
            &crate::test_connection_id(conn_id),
            TestMessage::Emit(tau_proto::Emit {
                event: Box::new(event),
                persist,
            }),
        )
        .expect("defer terminal output");
    }

    assert!(!event_log_contains_source_event(&h, conn_id, |event| {
        matches!(event, Event::TermBell(_) | Event::Osc1337SetUserVar(_))
    }));

    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(Default::default()),
    )
    .expect("activate terminal-output owner");

    let committed: Vec<_> = {
        let mut names = Vec::new();
        let mut seq = path_crate_event_log::EventLogSeq::new(0);
        while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
            seq = entry.seq.next();
            if entry.source.as_deref() == Some(conn_id)
                && matches!(
                    entry.event,
                    Event::TermBell(_) | Event::Osc1337SetUserVar(_)
                )
            {
                names.push(entry.event.name());
            }
        }
        names
    };
    assert_eq!(
        committed,
        [
            tau_proto::EventName::TERM_BELL,
            tau_proto::EventName::TERM_OSC1337_SET_USER_VAR,
        ]
    );
    let delivered: Vec<_> = observer
        .lock()
        .expect("observer")
        .iter()
        .filter_map(|routed| match &routed.frame {
            HarnessOutputMessage::Deliver(delivery)
                if matches!(
                    delivery.event.as_ref(),
                    Event::TermBell(_) | Event::Osc1337SetUserVar(_)
                ) =>
            {
                Some((
                    routed.source_id.clone(),
                    delivery.replay,
                    delivery.event.name(),
                ))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        delivered,
        [
            (
                Some(crate::test_connection_id(conn_id)),
                false,
                tau_proto::EventName::TERM_BELL,
            ),
            (
                Some(crate::test_connection_id(conn_id)),
                false,
                tau_proto::EventName::TERM_OSC1337_SET_USER_VAR,
            ),
        ]
    );

    h.shutdown().expect("shutdown");
}

/// Disconnecting the last per-agent context waiter must resume a prompt that
/// already committed but was deferred before its model snapshot was frozen.
#[test]
fn context_provider_disconnect_resumes_publish_idle_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    h.config.selected_model = Some("test/model".into());
    let conn_id = "disconnecting-agent-context";
    let _sink = connect_handshaking_tool(&mut h, conn_id);
    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
        }),
    )
    .expect("subscribe");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtensionContextProviderRegister(
            tau_proto::ExtensionContextProviderRegister {},
        )),
    )
    .expect("register context provider");
    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(Default::default()),
    )
    .expect("ready");

    h.dispatch_user_prompt(test_session_id("s1"), "resume after disconnect".to_owned())
        .expect("dispatch user prompt");
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );
    assert!(!h.runtime_io.publication.idle_dispatches.is_empty());
    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .values()
        .find_map(|agent| agent.identity.agent_id.clone())
        .map(|agent_id| tau_proto::AgentId::parse(&agent_id).expect("agent id"))
        .expect("loaded agent");
    let initialization_id = h.prompt_coordination.context_discovery.pending_agents[&agent_id]
        .initialization_id
        .clone();
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtAgentContextPublish(
            tau_proto::ExtAgentContextPublish {
                session_id: test_session_id("s1"),
                agent_initialization_id: initialization_id,

                agent_id: agent_id.clone(),
                key: "disconnect-test".into(),
                value: tau_proto::AgentContextValue(serde_json::json!("stale")),
            },
        )),
    )
    .expect("publish context before disconnect");
    assert!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(&agent_id))
            .to_string()
            .contains("stale")
    );

    h.handle_disconnect(&crate::test_connection_id(conn_id));

    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    assert!(
        !h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(&agent_id))
            .to_string()
            .contains("stale")
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt_context_contains(prompt, "resume after disconnect")
    )));
}

/// Staged captured-model presence followed by final absence is coalesced into
/// one authoritative removal without exposing the intermediate route.
#[test]
fn provider_ready_coalesces_staged_model_snapshots_to_final_state() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let mut captured_info = h
        .provider_runtime
        .model_info
        .values()
        .next()
        .expect("startup provider model")
        .clone();
    clear_quiet_provider_models(&mut h);
    let conn_id = "provider-staged-model-replacement";
    let sink = connect_handshaking_extension(&mut h, conn_id, tau_proto::ClientKind::Provider);
    let captured: tau_proto::ModelId = "staged/captured".into();
    captured_info.id = captured.clone();
    let cid = ensure_test_user_agent(&mut h);
    let (agent_id, transaction_id, checkpoint_prompt_id, through) =
        seed_restored_compaction_checkpoint(&mut h, &cid, &captured, "ct-staged-snapshot");

    for models in [vec![captured_info], Vec::new()] {
        h.handle_extension_event(
            conn_id,
            TestProtocolItem::Event(Event::ProviderModelsDeclared(
                tau_proto::ProviderModelsDeclared { models },
            )),
        )
        .expect("stage model snapshot");
    }
    assert!(!h.provider_runtime.model_info.contains_key(&captured));
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .enqueued_inference_checkpoints
            .is_empty(),
        "pre-Ready snapshots must not reconcile restored work"
    );

    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("activate provider");
    assert!(
        !h.provider_runtime.model_info.contains_key(&captured),
        "an intermediate staged route must never become the active provider route"
    );
    assert!(!h.provider_runtime.model_routes.contains_key(&captured));
    assert!(
        !h.prompt_coordination
            .compaction_runtime
            .enqueued_inference_checkpoints
            .contains(&(agent_id.clone(), transaction_id.clone()))
    );
    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentInferenceDispatchStarted(checkpoint)
                    if checkpoint.transaction_id.as_ref() == Some(&transaction_id)
                        && checkpoint.agent_prompt_id == checkpoint_prompt_id
                        && checkpoint.model.as_ref() == Some(&captured)
                        && checkpoint.operation == Some(tau_proto::PromptOperation::Inference)
                        && checkpoint.activation_cut == Some(tau_proto::AgentHead::Root)
                        && checkpoint.through == through
            ))
            .count(),
        1,
        "authoritative final absence commits one fully qualified checkpoint"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == checkpoint_prompt_id
                        && response.stop_reason == tau_proto::ProviderStopReason::Error
            ))
            .count(),
        1,
        "the unavailable captured route has one terminal response"
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(prompt) if prompt.agent_prompt_id == checkpoint_prompt_id
    )));
    assert!(
        sink.lock().expect("provider sink").iter().all(|routed| {
            !matches!(
                peel_inner_event(&routed.frame),
                Some(Event::AgentPromptCreated(prompt))
                    if prompt.agent_prompt_id == checkpoint_prompt_id
            )
        }),
        "authoritative final absence must not send provider work"
    );
}

/// An intermediate staged absence cannot terminalize restored work when the
/// provider's final Ready snapshot contains the captured model.
#[test]
fn provider_ready_coalesces_staged_absence_to_captured_route_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let mut captured_info = h
        .provider_runtime
        .model_info
        .values()
        .next()
        .expect("startup provider model")
        .clone();
    clear_quiet_provider_models(&mut h);
    let captured: tau_proto::ModelId = "staged/captured-final".into();
    captured_info.id = captured.clone();
    captured_info.supported_tool_types = vec![tau_proto::ToolType::Function];
    captured_info.efforts = vec![tau_proto::Effort::High];
    captured_info.verbosities = vec![tau_proto::Verbosity::Low];
    captured_info.thinking_summaries = vec![tau_proto::ThinkingSummary::Detailed];
    let current: tau_proto::ModelId = "other/current-selection".into();
    let mut current_info = staged_provider_model("other/current-selection");
    current_info.supported_tool_types.clear();
    h.provider_runtime
        .model_info
        .insert(current.clone(), current_info);
    h.provider_runtime
        .model_routes
        .insert(current.clone(), crate::test_connection_id("other-provider"));
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role");
    role.effort = Some(tau_proto::Effort::High);
    role.verbosity = Some(tau_proto::Verbosity::Low);
    role.thinking_summary = Some(tau_proto::ThinkingSummary::Detailed);
    role.tools = Some(vec![ToolName::new("captured_only_tool")]);

    let conn_id = "provider-staged-absence-replacement";
    let sink = connect_handshaking_extension(&mut h, conn_id, tau_proto::ClientKind::Provider);
    let tool_conn_id = "tool-staged-absence-replacement";
    connect_handshaking_tool(&mut h, tool_conn_id);
    h.handle_extension_event(
        tool_conn_id,
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("captured_only_tool"),
                tool_group: None,
                prompt_fragment: None,
            },
        )),
    )
    .expect("stage captured tool");
    let cid = ensure_test_user_agent(&mut h);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .identity
        .model_override = Some(current);
    let (agent_id, transaction_id, checkpoint_prompt_id, through) =
        seed_restored_compaction_checkpoint(&mut h, &cid, &captured, "ct-staged-captured-final");

    for models in [Vec::new(), vec![captured_info]] {
        h.handle_extension_event(
            conn_id,
            TestProtocolItem::Event(Event::ProviderModelsDeclared(
                tau_proto::ProviderModelsDeclared { models },
            )),
        )
        .expect("stage model snapshot");
    }
    h.handle_extension_message(
        &crate::test_connection_id(tool_conn_id),
        TestMessage::Ready(Default::default()),
    )
    .expect("tool Ready waits on provider");
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentInferenceDispatchStarted(checkpoint)
            if checkpoint.transaction_id.as_ref() == Some(&transaction_id)
    )));

    h.handle_extension_message(
        &crate::test_connection_id(conn_id),
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("activate provider");

    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentInferenceDispatchStarted(checkpoint)
                    if checkpoint.transaction_id.as_ref() == Some(&transaction_id)
                        && checkpoint.agent_prompt_id == checkpoint_prompt_id
                        && checkpoint.agent_id == agent_id
                        && checkpoint.model.as_ref() == Some(&captured)
                        && checkpoint.operation == Some(tau_proto::PromptOperation::Inference)
                        && checkpoint.activation_cut == Some(tau_proto::AgentHead::Root)
                        && checkpoint.through == through
            ))
            .count(),
        1
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ProviderResponseFinished(response)
            if response.agent_prompt_id == checkpoint_prompt_id
    )));
    let prompt = events
        .iter()
        .find_map(|event| match event {
            Event::AgentPromptCreated(prompt) if prompt.agent_prompt_id == checkpoint_prompt_id => {
                Some(prompt)
            }
            _ => None,
        })
        .expect("captured continuation prompt");
    assert_eq!(prompt.model, captured);
    assert_eq!(prompt.model_params.effort, tau_proto::Effort::High);
    assert_eq!(prompt.model_params.verbosity, tau_proto::Verbosity::Low);
    assert_eq!(
        prompt.model_params.thinking_summary,
        tau_proto::ThinkingSummary::Detailed
    );
    assert_eq!(
        prompt
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["captured_only_tool"]
    );
    assert_eq!(prompt.agent_id, agent_id);
    assert_eq!(prompt.session_id, h.session_runtime.current_session_id);
    assert_eq!(prompt.originator, tau_proto::PromptOriginator::User);
    assert_eq!(
        h.prompt_coordination
            .prompt_runtime
            .models
            .get(&checkpoint_prompt_id),
        Some(&captured)
    );
    assert!(
        sink.lock().expect("provider sink").iter().any(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::AgentPromptCreated(sent))
                    if sent.agent_prompt_id == checkpoint_prompt_id
                        && sent.model == captured
            )
        }),
        "the exact captured route receives the provider request"
    );
}

#[test]
fn tool_unregister_removes_tool_from_future_prompt() {
    // Regression: an explicit ToolUnregistrationDeclared must update the live
    // registry used for future prompt assembly while leaving old prompt
    // snapshots intact.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "before unregister");
    let before_spid = h.send_prompt_to_agent("s1");
    let before_prompt = read_prompt_created(&h, &before_spid);
    assert!(prompt_has_tool(&before_prompt, "shell"));
    h.handle_provider_response_finished(provider_text_response(
        &before_spid,
        before_prompt.agent_id.clone(),
        "before unregister complete",
    ))
    .expect("finish first prompt");

    unregister_shell(&mut h);

    append_user_message_via_event(&mut h, "s1", "after unregister");
    let after_spid = h.send_prompt_to_agent("s1");
    let after_prompt = read_prompt_created(&h, &after_spid);

    assert!(prompt_has_tool(&before_prompt, "shell"));
    assert!(!prompt_has_tool(&after_prompt, "shell"));

    h.shutdown().expect("shutdown");
}

/// An assigned prefix fails closed when an old or raw client attempts to
/// register unprefixed structural identifiers.
#[test]
fn assigned_tool_prefix_rejects_unprefixed_registration() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    connect_handshaking_tool(&mut h, "prefixed-extension");
    h.extensions
        .entries
        .get_mut("prefixed-extension")
        .expect("extension")
        .tool_prefix = Some(tau_proto::ToolNamePrefix::parse("work").expect("prefix"));

    for (name, alias, group) in [
        ("slack_send", Some("work_visible"), Some("work_slack")),
        ("work_slack_send", Some("visible"), Some("work_slack")),
        ("work_slack_send", Some("work_visible"), Some("slack")),
    ] {
        let mut spec = staged_tool_spec(name);
        spec.model_visible_name = alias.map(ToolName::new);
        h.handle_extension_event(
            "prefixed-extension",
            TestProtocolItem::Event(Event::ToolRegistrationDeclared(
                tau_proto::ToolRegistrationDeclared {
                    tool: spec,
                    tool_group: group.map(|name| tau_proto::ToolGroup {
                        name: tau_proto::ToolGroupName::new(name),
                        prompt_fragment: None,
                    }),
                    prompt_fragment: None,
                },
            )),
        )
        .expect("handle registration");
        assert!(h.tool_routing.registry.providers_for(name).is_empty());
    }
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    let mut saw_notice = false;
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches!(
            &entry.event,
            Event::HarnessNotice(notice)
                if notice.message.contains("assigned tool_prefix `work`")
        ) {
            saw_notice = true;
            break;
        }
    }
    assert!(
        saw_notice,
        "events: {:?}",
        event_log_events(&h)
            .into_iter()
            .map(|event| event.name().to_string())
            .collect::<Vec<_>>()
    );
    h.shutdown().expect("shutdown");
}

#[test]
fn unavailable_tool_is_reported_without_crashing() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let removed = h
        .tool_routing
        .registry
        .unregister_connection(&crate::test_connection_id(&conn_id));
    assert!(removed.iter().any(|t| t == "shell"));

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-x"), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "shell printf hi".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    let target_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-x"),
        agent_id: target_agent_id.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("unavailable tool should be rejected cleanly");

    let expected_prefix = unavailable_tool_error_message(&ToolName::new("shell"));
    assert!(default_agent_tree(&h).nodes().iter().any(|node| {
        matches!(
            &node.entry,
            AgentEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message } if message.starts_with(&expected_prefix)
                        )
                })
        )
    }));
    let followup_prompt = read_nth_prompt_created(&h, 0);
    assert!(
        followup_prompt
            .context
            .flatten()
            .iter()
            .any(|item| matches!(item, ContextItem::ToolResult(_))),
        "follow-up prompt should include the persisted tool error as a tool_result item"
    );
    h.shutdown().expect("shutdown");
}
#[test]
fn hello_protocol_version_mismatch_is_rejected() {
    let hello = tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION + 1,
        client_name: crate::test_extension_name("future-client"),
        client_kind: tau_proto::ClientKind::Tool,
        expected_session_id: None,
        capabilities: Default::default(),
    };

    let error = validate_protocol_version(&hello).expect_err("reject mismatched protocol");
    assert!(
        error
            .to_string()
            .contains("unsupported protocol version from future-client"),
        "unexpected error: {error}"
    );
}

#[test]
fn required_initial_config_error_emits_diagnostic_then_fails_startup() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    let conn_id = "required-config-bad-ext";
    connect_handshaking_tool(&mut h, conn_id);

    let error = h
        .handle_extension_message(
            &crate::test_connection_id(conn_id),
            TestMessage::ConfigError(tau_proto::ConfigError {
                message: "required setting is invalid".to_owned(),
            }),
        )
        .expect_err("required initial config rejection is startup-fatal");

    assert!(error.to_string().contains("required extension"));
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(notice)
                if notice.kind == tau_proto::notice_kind::EXTENSION_CONFIG_ERROR
                    && notice.message.contains("required setting is invalid")
        )
    ));
    h.shutdown().expect("shutdown");
}

#[test]
fn required_startup_timeout_remains_startup_timeout() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "required-timeout-ext";
    let _extension_sink = connect_handshaking_tool(&mut h, conn_id);

    let error = h
        .handle_extensions_startup_timeout()
        .expect_err("required blocker should keep startup timeout behavior");

    assert!(matches!(error, HarnessError::StartupTimeout));
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.message.contains("startup timed out waiting for required extension")
                    && info.message.contains("required-timeout-ext")
        )
    ));
}

/// Protocol-version mismatches preserve required/optional extension
/// availability policy.
#[test]
fn optional_mismatched_protocol_is_disabled_but_required_mismatch_is_fatal() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    for (connection_id, required) in [("optional-old", false), ("required-old", true)] {
        connect_handshaking_tool(&mut h, connection_id);
        let entry = h
            .extensions
            .entries
            .get_mut(connection_id)
            .expect("extension");
        entry.state = ExtensionState::Spawning;
        entry.require = required;
    }
    let mismatched_hello = |name: &str| {
        TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION + 1,
            client_name: crate::test_extension_name(name),
            client_kind: tau_proto::ClientKind::Tool,
            expected_session_id: None,
            capabilities: Default::default(),
        })
    };

    h.handle_extension_message(
        &crate::test_connection_id("optional-old"),
        mismatched_hello("optional-old"),
    )
    .expect("optional mismatched peer is disabled");
    assert_eq!(
        h.extensions.entries["optional-old"].state,
        ExtensionState::Disconnected
    );
    let error = h
        .handle_extension_message(
            &crate::test_connection_id("required-old"),
            mismatched_hello("required-old"),
        )
        .expect_err("required mismatched peer is fatal");
    assert!(error.to_string().contains("protocol"));
    h.shutdown().expect("shutdown");
}

/// Pre-activation storage is bounded by both retained frame count and encoded
/// bytes; optional overflow isolates the claimant instead of growing without
/// bound.
#[test]
fn optional_activation_staging_enforces_count_and_byte_quotas() {
    let make_harness = |connection_id: &str| {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        h.extensions.initial_tool_preflight_complete = false;
        connect_handshaking_tool(&mut h, connection_id);
        h.extensions
            .entries
            .get_mut(connection_id)
            .expect("extension")
            .require = false;
        (td, h)
    };

    let (_count_td, mut count_harness) = make_harness("count-overflow");
    let subscribe = || {
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: Vec::new(),
        })
    };
    let (_diagnostic_td, mut diagnostic_harness) = make_harness("diagnostic-at-limit");
    for _ in 0..MAX_EXTENSION_ACTIVATION_MESSAGES {
        diagnostic_harness
            .handle_extension_message(
                &crate::test_connection_id("diagnostic-at-limit"),
                subscribe(),
            )
            .expect("message within count limit");
    }
    diagnostic_harness
        .handle_extension_message(
            &crate::test_connection_id("diagnostic-at-limit"),
            TestMessage::ConfigError(tau_proto::ConfigError {
                message: "configuration rejected at quota boundary".to_owned(),
            }),
        )
        .expect("mandatory config diagnostic bypasses retained-message quota");
    assert!(event_log_contains_source_event(
        &diagnostic_harness,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(notice)
                if notice.kind == tau_proto::notice_kind::EXTENSION_CONFIG_ERROR
                    && notice.message.contains("configuration rejected at quota boundary")
        )
    ));
    diagnostic_harness.shutdown().expect("shutdown");

    let (_ready_td, mut ready_harness) = make_harness("ready-at-limit");
    ready_harness
        .extension_activation_stage_mut(&crate::test_connection_id("ready-at-limit"))
        .retained_message_count = MAX_EXTENSION_ACTIVATION_MESSAGES;
    ready_harness
        .handle_extension_message(
            &crate::test_connection_id("ready-at-limit"),
            TestMessage::Ready(Default::default()),
        )
        .expect("Ready does not consume retained-message quota");
    assert_eq!(
        ready_harness.extensions.entries["ready-at-limit"].state,
        ExtensionState::Ready
    );
    ready_harness.shutdown().expect("shutdown");

    let (_oversized_diagnostic_td, mut oversized_diagnostic_harness) =
        make_harness("oversized-diagnostic");
    oversized_diagnostic_harness
        .handle_extension_message(
            &crate::test_connection_id("oversized-diagnostic"),
            TestMessage::ConfigError(tau_proto::ConfigError {
                message: "x".repeat(MAX_EXTENSION_ACTIVATION_BYTES + 1),
            }),
        )
        .expect("oversized config diagnostic is bounded and emitted");
    assert!(event_log_contains_source_event(
        &oversized_diagnostic_harness,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(notice)
                if notice.kind == tau_proto::notice_kind::EXTENSION_CONFIG_ERROR
                    && notice.message.contains("[truncated]")
                    && notice.message.len()
                        < super::super::super::MAX_EXTENSION_CONFIG_ERROR_BYTES * 2
        )
    ));
    oversized_diagnostic_harness.shutdown().expect("shutdown");

    for _ in 0..MAX_EXTENSION_ACTIVATION_MESSAGES {
        count_harness
            .handle_extension_message(&crate::test_connection_id("count-overflow"), subscribe())
            .expect("message within count limit");
    }
    count_harness
        .handle_extension_message(&crate::test_connection_id("count-overflow"), subscribe())
        .expect("count overflow degrades");
    assert_eq!(
        count_harness.extensions.entries["count-overflow"].state,
        ExtensionState::Disconnected
    );
    count_harness.shutdown().expect("shutdown");

    let oversized = || {
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Prefix(
                "x".repeat(MAX_EXTENSION_ACTIVATION_BYTES + 1),
            )],
        })
    };
    let (_bytes_td, mut bytes_harness) = make_harness("bytes-overflow");
    bytes_harness
        .handle_extension_message(&crate::test_connection_id("bytes-overflow"), oversized())
        .expect("byte overflow degrades");
    assert_eq!(
        bytes_harness.extensions.entries["bytes-overflow"].state,
        ExtensionState::Disconnected
    );
    bytes_harness.shutdown().expect("shutdown");

    let (_required_td, mut required_harness) = make_harness("required-overflow");
    required_harness
        .extensions
        .entries
        .get_mut("required-overflow")
        .expect("required")
        .require = true;
    required_harness
        .handle_extension_message(&crate::test_connection_id("required-overflow"), oversized())
        .expect_err("required initial overflow is fatal");
    required_harness.shutdown().expect("shutdown");

    let (_runtime_td, mut runtime_harness) = make_harness("runtime-overflow");
    runtime_harness.extensions.initial_tool_preflight_complete = true;
    runtime_harness
        .handle_extension_message(&crate::test_connection_id("runtime-overflow"), oversized())
        .expect("runtime overflow isolates");
    let runtime_entry = &runtime_harness.extensions.entries["runtime-overflow"];
    assert_eq!(runtime_entry.state, ExtensionState::Disconnected);
    assert!(runtime_entry.respawn_allowed);
    runtime_harness.shutdown().expect("shutdown");
}

#[test]
fn prompt_snapshot_does_not_expand_to_staged_registration() {
    // Regression: a staged registration is not part of the prompt snapshot
    // until Ready. If an old prompt calls such an unadvertised staged tool, the
    // harness must close it as unavailable instead of waiting and effectively
    // adding the staged tool to the original prompt.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let staged_sink = connect_handshaking_tool(&mut h, "conn-unadvertised-staged-tool");
    h.handle_extension_event(
        "conn-unadvertised-staged-tool",
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("unadvertised_staged_tool"),
                tool_group: None,
                prompt_fragment: None,
            },
        )),
    )
    .expect("stage tool");

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-unadvertised-staged");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-unadvertised-staged"), cid.clone());
    assert!(
        !h.prompt_coordination.prompt_runtime.tool_specs
            [&test_agent_prompt_id("sp-unadvertised-staged")]
            .iter()
            .any(|spec| spec.name == "unadvertised_staged_tool")
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-unadvertised-staged"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-unadvertised".into(),
            name: ToolName::new("unadvertised_staged_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("unadvertised staged call handled");

    let expected = unavailable_tool_error_message(&ToolName::new("unadvertised_staged_tool"));
    assert!(default_agent_tree(&h).nodes().iter().any(|node| {
        matches!(
            &node.entry,
            AgentEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id.as_str() == "call-unadvertised"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message } if message == &expected
                        )
                })
        )
    }));
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);
    assert!(!sink_has_tool_invoke(&staged_sink, "call-unadvertised"));

    h.handle_extension_message(
        &crate::test_connection_id("conn-unadvertised-staged-tool"),
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");
    assert!(!sink_has_tool_invoke(&staged_sink, "call-unadvertised"));

    h.shutdown().expect("shutdown");
}

#[test]
fn all_non_declaration_events_wait_for_the_global_activation_barrier() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    connect_handshaking_tool(&mut h, "operational-owner");
    connect_handshaking_tool(&mut h, "activation-blocker");
    h.ui_runtime.pending_ui_shell_commands.insert(
        UiShellRouteId::new(test_shell_command_id("startup-shell")),
        PendingUiShellCommand {
            provider_id: crate::test_connection_id("operational-owner"),
            command: tau_proto::UiShellCommand {
                command_id: tau_proto::ShellCommandId::parse("startup-shell")
                    .expect("test identifier must satisfy its grammar"),
                session_id: test_session_id("s1"),
                command: "printf held".to_owned(),
                include_in_context: false,
                target_agent_id: None,
            },
            targets_ephemeral: false,
        },
    );

    h.handle_extension_event(
        "operational-owner",
        TestProtocolItem::Event(Event::ShellCommandFinishedReported(
            tau_proto::ShellCommandFinished {
                command_id: tau_proto::ShellCommandId::parse("startup-shell")
                    .expect("test identifier must satisfy its grammar"),
                session_id: test_session_id("s1"),
                command: "printf held".to_owned(),
                include_in_context: false,
                target_agent_id: None,
                output: "held".to_owned(),
                exit_code: Some(0),
                cancelled: false,
            },
        )),
    )
    .expect("defer shell completion");
    h.handle_extension_message(
        &crate::test_connection_id("operational-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("owner ready");

    assert!(!event_log_contains_source_event(
        &h,
        "operational-owner",
        |event| matches!(
            event,
            Event::ShellCommandFinishedReported(finished)
                if finished.command_id.as_str() == "startup-shell"
        )
    ));

    h.handle_extension_message(
        &crate::test_connection_id("activation-blocker"),
        TestMessage::Ready(Default::default()),
    )
    .expect("complete barrier");

    assert!(event_log_contains_source_event(
        &h,
        "operational-owner",
        |event| matches!(
            event,
            Event::ShellCommandFinishedReported(finished)
                if finished.command_id.as_str() == "startup-shell"
        )
    ));
    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ShellCommandFinished(finished)
                if finished.command_id.as_str() == "startup-shell"
        )
    ));
    h.shutdown().expect("shutdown");
}

#[test]
fn old_prompt_call_gets_tau_internal_unavailable_error() {
    // Regression: a prompt that was created before unregister can still contain
    // the old tool definition. If the agent calls it after the provider removed
    // the tool, the harness must close the call with an internal tool error.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "use shell");
    let spid = h.send_prompt_to_agent("s1");
    let old_prompt = read_prompt_created(&h, &spid);
    assert!(prompt_has_tool(&old_prompt, "shell"));

    unregister_shell(&mut h);

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("unavailable old tool call should be closed");

    let expected_prefix = unavailable_tool_error_message(&ToolName::new("shell"));
    assert!(default_agent_tree(&h).nodes().iter().any(|node| {
        matches!(
            &node.entry,
            AgentEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message } if message.starts_with(&expected_prefix)
                        )
                })
        )
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn unregister_queues_unavailable_notice_for_next_user_prompt_only() {
    // Availability notices are hidden context for the next real user turn, not
    // standalone internal prompts dispatched at unregister time.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let notice = tool_unavailable_notice_prompt(&ToolName::new("shell"));
    unregister_shell(&mut h);

    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );
    assert_eq!(agent_prompt_text_count(&h, &notice), 0);

    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("after unregister".to_owned()))
        .expect("dispatch user prompt");

    let prompt = read_nth_prompt_created(&h, 0);
    let notice_pos = prompt
        .context
        .flatten()
        .iter()
        .position(|item| {
            context_text(item) == Some(crate::internal_envelope::frame(&notice).as_str())
        })
        .expect("availability notice in prompt");
    let user_pos = prompt
        .context
        .flatten()
        .iter()
        .position(|item| context_text(item) == Some("after unregister"))
        .expect("user prompt in prompt");
    assert!(notice_pos < user_pos);
    assert_eq!(agent_prompt_text_count(&h, &notice), 1);

    h.shutdown().expect("shutdown");
}

#[test]
fn reregister_before_notice_delivery_dequeues_unavailable_notice() {
    // A quick unregister/register pair should be invisible to the model.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let spec = shell_tool_spec(&h);
    let notice = tool_unavailable_notice_prompt(&ToolName::new("shell"));
    unregister_shell(&mut h);
    reregister_shell(&mut h, spec);

    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("after reconnect".to_owned()))
        .expect("dispatch user prompt");

    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(context_text_count(&prompt, &notice), 0);
    assert_eq!(agent_prompt_text_count(&h, &notice), 0);
    assert!(prompt_has_tool(&prompt, "shell"));

    h.shutdown().expect("shutdown");
}

#[test]
fn reregister_after_notice_delivery_queues_available_again_notice() {
    // Once the model has been told a tool disappeared, the matching
    // re-registration needs a hidden available-again notice on the next user
    // turn so the model can trust the refreshed tool list.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let spec = shell_tool_spec(&h);
    let unavailable = tool_unavailable_notice_prompt(&ToolName::new("shell"));
    let available = tool_available_again_notice_prompt(&ToolName::new("shell"));
    unregister_shell(&mut h);

    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("after unregister".to_owned()))
        .expect("dispatch unavailable prompt");
    let first_prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(
        context_text_count(
            &first_prompt,
            &crate::internal_envelope::frame(&unavailable)
        ),
        1
    );
    h.handle_provider_response_finished(provider_text_response(
        &first_prompt.agent_prompt_id,
        first_prompt.agent_id.clone(),
        "acknowledged",
    ))
    .expect("finish first checkpointed prompt");

    reregister_shell(&mut h, spec);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("after reregister".to_owned()))
        .expect("dispatch available prompt");

    let second_prompt = read_nth_prompt_created(&h, 1);
    let available_pos = second_prompt
        .context
        .flatten()
        .iter()
        .position(|item| {
            context_text(item) == Some(crate::internal_envelope::frame(&available).as_str())
        })
        .expect("available-again notice in prompt");
    let user_pos = second_prompt
        .context
        .flatten()
        .iter()
        .position(|item| context_text(item) == Some("after reregister"))
        .expect("user prompt in prompt");
    assert!(available_pos < user_pos);
    assert_eq!(agent_prompt_text_count(&h, &available), 1);
    assert!(prompt_has_tool(&second_prompt, "shell"));

    h.shutdown().expect("shutdown");
}
/// Ensures a recovery Tau-state policy publishes its dedicated replayable
/// notice kind rather than sharing generic harness-internal warnings.
#[test]
fn state_access_startup_diagnostic_uses_dedicated_notice_kind() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");

    h.emit_extension_startup_diagnostics(&[crate::settings::ExtensionStartupDiagnostic {
        extension: "core-shell".to_owned(),
        message: "extension `core-shell` uses Tau-state access `legacy` from global harness configuration"
            .to_owned(),
        kind: ExtensionStartupDiagnosticKind::StateAccess {
            source: TauStateAccessSource::GlobalConfiguration,
        },
    }]);

    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_STATE_ACCESS
                    && info.purpose == tau_proto::NoticePurpose::Alert
                    && info.message.contains("core-shell")
        )
    ));
}

#[test]
fn harness_failure_notice_is_mandatory_warning() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");

    h.emit_harness_failure("failed to dispatch queued prompt: boom");

    assert!(event_log_contains_source_event(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.kind == tau_proto::notice_kind::HARNESS_FAILURE
                    && info.level == tau_proto::NoticeLevel::Warning
                    && info.purpose == tau_proto::NoticePurpose::Alert
                    && info.message == "failed to dispatch queued prompt: boom"
        )
    ));
}

/// Reader decode failures retain their provenance through startup and runtime
/// policy instead of being mistaken for compatibility EOF disconnects.
#[test]
fn decode_failures_follow_required_optional_and_runtime_policy() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.extensions.initial_tool_preflight_complete = false;
    for (connection_id, required) in [
        ("optional-decode", false),
        ("required-decode", true),
        ("runtime-decode", true),
    ] {
        connect_handshaking_tool(&mut h, connection_id);
        h.extensions
            .entries
            .get_mut(connection_id)
            .expect("extension")
            .require = required;
    }

    h.handle_startup_read_failure(
        &crate::test_connection_id("optional-decode"),
        "malformed cbor".to_owned(),
    )
    .expect("optional decode failure degrades");
    assert_eq!(
        h.extensions.entries["optional-decode"].state,
        ExtensionState::Disconnected
    );
    assert!(!h.extensions.entries["optional-decode"].respawn_allowed);

    let error = h
        .handle_startup_read_failure(
            &crate::test_connection_id("required-decode"),
            "oversized frame".to_owned(),
        )
        .expect_err("required initial decode failure is fatal");
    assert!(error.to_string().contains("protocol decode failed"));

    h.extensions.initial_tool_preflight_complete = true;
    h.extensions
        .entries
        .get_mut("runtime-decode")
        .expect("runtime extension")
        .state = ExtensionState::Ready;
    let mut served_clients = 0;
    let mut exit_on_disconnect = false;
    let mut ever_attached = false;
    h.handle_runtime_event(
        HarnessEvent::ReadFailed {
            connection_id: crate::test_connection_id("runtime-decode"),
            error: "malformed cbor".to_owned(),
        },
        &mut served_clients,
        &mut exit_on_disconnect,
        &mut ever_attached,
    )
    .expect("runtime decode failure is isolated");
    let runtime = &h.extensions.entries["runtime-decode"];
    assert_eq!(runtime.state, ExtensionState::Disconnected);
    assert!(runtime.respawn_allowed);
    h.shutdown().expect("shutdown");
}
/// Restart-disable diagnostics retain their actionable suffix while bounding
/// maximum valid and defensive overlong UTF-8 names.
#[test]
fn restart_disabled_notice_is_bounded_and_utf8_safe() {
    for name in [
        "x".repeat(tau_proto::EXTENSION_NAME_MAX_BYTES),
        "é".repeat(MAX_EXTENSION_RESTART_NOTICE_BYTES),
    ] {
        let notice = extension_restart_disabled_notice(&name);
        assert!(notice.len() <= MAX_EXTENSION_RESTART_NOTICE_BYTES);
        assert!(
            notice
                .ends_with("automatic restart attempts; it remains disconnected for this session")
        );
        assert!(notice.contains(&MAX_EXTENSION_RESTART_ATTEMPTS.to_string()));
        if name.len() == tau_proto::EXTENSION_NAME_MAX_BYTES {
            assert!(notice.contains(&name));
        } else {
            assert!(notice.contains('…'));
        }
    }
}

/// A late event-loop wake still spaces failed spawn attempts from the actual
/// processing time instead of draining overdue retry cohorts back-to-back.
#[test]
fn failed_spawn_retry_delay_anchors_to_late_fake_clock_now() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let connection_id = "missing-restart-command";
    let _sink = connect_handshaking_tool(&mut h, connection_id);
    let mut config = supervised_test_config(connection_id, "exit 1");
    config.command = td.path().join("missing-extension").display().to_string();
    let entry = h
        .extensions
        .entries
        .get_mut(connection_id)
        .expect("extension");
    entry.state = ExtensionState::Disconnected;
    entry.supervised_config = Some(config);
    let scheduled_at = Instant::now();
    h.schedule_extension_restart_at(&crate::test_connection_id(connection_id), scheduled_at);
    let first_deadline = h.extensions.restart_deadlines[connection_id];
    let late_now = first_deadline + Duration::from_secs(10);

    h.process_runtime_deadlines_at(late_now);

    assert_eq!(h.extensions.entries[connection_id].restart_attempt, 1);
    assert_eq!(
        h.extensions.restart_deadlines[connection_id],
        late_now + EXTENSION_RESTART_DELAY
    );
    h.process_runtime_deadlines_at(late_now + EXTENSION_RESTART_DELAY);
    assert_eq!(h.extensions.entries[connection_id].restart_attempt, 2);
    h.shutdown().expect("shutdown");
}

/// Runtime replacement is gated on writer completion: the cleanup watchdog
/// first kills and reaps a non-reading child, then the one-second restart
/// deadline becomes eligible.
#[test]
fn nonreading_child_is_reaped_before_delayed_replacement() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let (connection_id, child_pid) = connect_supervised_test_process(
        &mut h,
        supervised_test_config("blocked-runtime-tool", "exec sleep 30"),
        tau_proto::ClientKind::Tool,
    );
    h.runtime_io
        .bus
        .send_to(
            &connection_id,
            None,
            HarnessOutputMessage::deliver(Event::HarnessNotice(tau_proto::HarnessNotice {
                kind: tau_proto::notice_kind::HARNESS_NOTICE.to_owned(),
                message: "x".repeat(2 * 1024 * 1024),
                level: tau_proto::NoticeLevel::Info,
                purpose: tau_proto::NoticePurpose::Diagnostic,
            })),
        )
        .expect("queue blocking frame");
    let disconnected_at = Instant::now();

    h.handle_disconnect_at(&connection_id, disconnected_at);

    assert!(h.extensions.supervised_writers.contains_key(&connection_id));
    assert!(!h.extensions.restart_deadlines.contains_key(&connection_id));
    let cleanup_at = disconnected_at + SUPERVISED_CLEANUP_GRACE;
    h.process_runtime_deadlines_at(cleanup_at);
    loop {
        let event = h
            .runtime_io
            .rx
            // The cleanup worker must terminate and reap the process group, then
            // let the blocked writer report completion. Two seconds is shorter
            // than that bounded asynchronous sequence under coverage.
            .recv_timeout(Duration::from_secs(10))
            .expect("cleanup-complete event");
        match h.expand_component_ingress_wake(event) {
            HarnessEvent::SupervisedWriterCleanupComplete {
                connection_id: completed,
            } if completed == connection_id => {
                h.handle_supervised_writer_cleanup_complete_at(&completed, cleanup_at)
                    .expect("join cleaned writer");
                break;
            }
            HarnessEvent::Disconnected { .. } | HarnessEvent::ReadFailed { .. } => {}
            _ => panic!("unexpected event while waiting for cleanup completion"),
        }
    }

    assert!(!process_is_signalable(child_pid));
    let restart_at = h.extensions.restart_deadlines[&connection_id];
    assert_eq!(restart_at, cleanup_at + EXTENSION_RESTART_DELAY);
    h.process_runtime_deadlines_at(restart_at - Duration::from_nanos(1));
    assert_eq!(h.extensions.entries[&connection_id].restart_attempt, 0);
    h.process_runtime_deadlines_at(restart_at);
    let replacement_id = h
        .extension_connection_id("blocked-runtime-tool")
        .expect("replacement connection")
        .to_owned();
    assert_ne!(replacement_id, connection_id.as_str());
    assert_eq!(
        h.extensions.entries[replacement_id.as_str()].restart_attempt,
        1
    );
    assert!(!process_is_signalable(child_pid));
    h.shutdown().expect("shutdown");
}

/// Shutdown arms all retained cleanup watchdogs before joining writers, so a
/// child that never reads stdin cannot leave its child or transport thread.
#[test]
fn shutdown_reaps_nonreading_supervised_children_and_joins_writers() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let mut children = Vec::new();
    for index in 0..3 {
        let name = format!("nonreading-tool-{index}");
        let (connection_id, child_pid) = connect_supervised_test_process(
            &mut h,
            supervised_test_config(&name, "exec sleep 30"),
            tau_proto::ClientKind::Tool,
        );
        h.runtime_io
            .bus
            .send_to(
                &connection_id,
                None,
                HarnessOutputMessage::deliver(Event::HarnessNotice(tau_proto::HarnessNotice {
                    kind: tau_proto::notice_kind::HARNESS_NOTICE.to_owned(),
                    message: "x".repeat(2 * 1024 * 1024),
                    level: tau_proto::NoticeLevel::Info,
                    purpose: tau_proto::NoticePurpose::Diagnostic,
                })),
            )
            .expect("queue large frame");
        children.push((connection_id, child_pid));
    }

    let started = Instant::now();
    h.shutdown().expect("shutdown");

    // Three serialized two-second grace windows exceed this broad bound;
    // parallel watchdog arming completes in one window.
    assert!(started.elapsed() < Duration::from_secs(5));
    for (_, child_pid) in &children {
        assert!(!process_is_signalable(*child_pid));
    }
    assert!(h.extensions.supervised_writers.is_empty());
    // Shutdown closes component ingress before joins. Reader lifecycle sends
    // therefore complete without requiring the stopped event loop to drain
    // one `Disconnected` payload per child.
    drop(children);
}

/// Overall shutdown must release harness transport resources even when an
/// in-process runner observes EOF but then blocks until the test releases it.
#[test]
fn shutdown_detaches_stuck_in_process_runner_after_shared_grace() {
    fn runner(reader: UnixStream, writer: UnixStream) -> Result<(), String> {
        crate::harness::run_echo_provider(reader, writer).map_err(|error| error.to_string())?;
        STUCK_PROVIDER_OBSERVED_TRANSPORT_CLOSE.store(true, Ordering::SeqCst);
        while !STUCK_PROVIDER_RELEASE.load(Ordering::SeqCst) {
            std::thread::park_timeout(Duration::from_millis(1));
        }
        STUCK_PROVIDER_EXITED.store(true, Ordering::SeqCst);
        Ok(())
    }

    STUCK_PROVIDER_OBSERVED_TRANSPORT_CLOSE.store(false, Ordering::SeqCst);
    STUCK_PROVIDER_RELEASE.store(false, Ordering::SeqCst);
    STUCK_PROVIDER_EXITED.store(false, Ordering::SeqCst);
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state_dir.join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    let mut h = Harness::new_with_provider(
        &state_dir,
        dirs,
        runner,
        Vec::new(),
        "s1",
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::Durable,
    )
    .expect("start");
    let (watchdog_cancel, watchdog_cancel_rx) = mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if watchdog_cancel_rx
            .recv_timeout(Duration::from_secs(4))
            .is_err()
        {
            STUCK_PROVIDER_RELEASE.store(true, Ordering::SeqCst);
        }
    });
    let _release = StuckProviderRelease {
        watchdog_cancel,
        watchdog: Some(watchdog),
    };

    let started = Instant::now();
    h.shutdown().expect("shutdown");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(STUCK_PROVIDER_OBSERVED_TRANSPORT_CLOSE.load(Ordering::SeqCst));
}

/// Ensures startup failures after the initial UI is accepted are delivered
/// through the connection's normal writer, avoiding unsynchronized side-channel
/// writes to the same protocol stream.
#[test]
fn accepted_initial_client_startup_error_uses_normal_writer() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let (server_end, client_end) = UnixStream::pair().expect("pair");
    let client_id = h.accept_client(server_end).expect("accept client");

    let error = path_std_io::Error::other("post-accept startup failure");
    h.send_startup_disconnect_to_initial_client(Some(&client_id), &error);

    let mut reader = HarnessOutputReader::new(BufReader::new(client_end));
    let message = reader
        .read_message()
        .expect("read startup disconnect")
        .expect("startup disconnect");
    let HarnessOutputMessage::Disconnect(disconnect) = message else {
        panic!("expected disconnect frame");
    };
    let reason = disconnect.reason.expect("disconnect reason");
    assert!(reason.contains("harness startup failed"));
    assert!(reason.contains("post-accept startup failure"));
}

/// The onboarding notice remains a directed live presentation frame: it reaches
/// only the initial UI, is absent when disabled, and never enters replay state.
#[test]
fn introduction_notice_is_enabled_directed_and_process_local() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let replayable_notices_before = h.runtime_io.replayable_harness_notices.len();
    let (initial_server, initial_client) = UnixStream::pair().expect("initial pair");
    let initial_id = h.accept_client(initial_server).expect("accept initial");
    let (attached_server, attached_client) = UnixStream::pair().expect("attached pair");
    h.accept_client(attached_server).expect("accept attachment");

    h.send_introduction_notice_to_initial_client(Some(&initial_id));

    let mut reader = HarnessOutputReader::new(BufReader::new(initial_client));
    let message = reader
        .read_message()
        .expect("read introduction")
        .expect("introduction frame");
    let HarnessOutputMessage::Deliver(delivery) = message else {
        panic!("expected delivery frame");
    };
    let Event::HarnessNotice(notice) = delivery.event() else {
        panic!("expected harness notice");
    };
    assert_eq!(notice.kind, tau_proto::notice_kind::HARNESS_INTRODUCTION);
    assert_eq!(
        notice.message,
        "Welcome to Tau! Ask your model to introduce you to Tau."
    );
    assert_eq!(notice.level, tau_proto::NoticeLevel::Info);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Diagnostic);
    assert_eq!(
        h.runtime_io.replayable_harness_notices.len(),
        replayable_notices_before
    );

    attached_client
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set attachment timeout");
    let error = HarnessOutputReader::new(BufReader::new(attached_client))
        .read_message()
        .expect_err("attachment must not receive introduction");
    assert!(matches!(
        error,
        tau_proto::DecodeError::Io(error)
            if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
    ));

    h.config.accepted_harness_settings.show_introduction_notice = false;
    let (disabled_server, disabled_client) = UnixStream::pair().expect("disabled pair");
    let disabled_id = h
        .accept_client(disabled_server)
        .expect("accept disabled UI");
    h.send_introduction_notice_to_initial_client(Some(&disabled_id));
    disabled_client
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set disabled timeout");
    let error = HarnessOutputReader::new(BufReader::new(disabled_client))
        .read_message()
        .expect_err("disabled notice must remain absent");
    assert!(matches!(
        error,
        tau_proto::DecodeError::Io(error)
            if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
    ));
    assert_eq!(
        h.runtime_io.replayable_harness_notices.len(),
        replayable_notices_before
    );
}
