//! Tests for agent messaging behavior.

use tau_config::settings::AgentWatchRetryNotificationPolicy;

use super::*;

#[test]
fn provider_owner_validation_rejects_provider_event_message_emit() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.handle_extension_event(
        "conn-wrong",
        TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ProviderResponseFinished(ProviderResponseFinished {
                automatic_compaction_decision: None,
                output_length_disposition: tau_proto::OutputLengthDisposition::None,
                estimated_api_cost_rates: None,
                estimated_api_cost_increment: None,

                agent_prompt_id: test_agent_prompt_id("spoofed-prompt"),
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                output_items: Vec::new(),
                stop_reason: tau_proto::ProviderStopReason::EndTurn,
                error: None,
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                usage: None,
                originator: tau_proto::PromptOriginator::User,
                compaction_original_input_tokens: None,
                compaction_output_tokens: None,
                backend: None,
                provider_attempt: Default::default(),
                provider_response_id: None,
                ws_pool_delta: None,
            })),
            persist: true,
        })),
    )
    .expect("emitted provider event ignored");

    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ProviderResponseFinished(response)
            if response.agent_prompt_id.as_str() == "spoofed-prompt"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn linear_agent_prompts_strictly_extend_previous_messages() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "hello");

    let spid1 = h.send_prompt_to_agent("s1");
    let prompt1 = read_prompt_created(&h, &spid1);

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid1,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "hi".to_owned(),
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
    .expect("persist first agent response");

    append_user_message_via_event(&mut h, "s1", "again");

    let spid2 = h.send_prompt_to_agent("s1");
    let prompt2 = read_prompt_created(&h, &spid2);

    assert_eq!(prompt2.system_prompt, prompt1.system_prompt);
    assert_eq!(prompt2.tools, prompt1.tools);
    assert_eq!(prompt2.model, prompt1.model);
    assert_eq!(prompt2.model_params, prompt1.model_params);
    assert!(
        prompt1.context.flatten().len() < prompt2.context.flatten().len(),
        "second prompt should strictly extend first: {} !< {}",
        prompt1.context.flatten().len(),
        prompt2.context.flatten().len()
    );
    assert_eq!(
        &prompt2.context.flatten()[..prompt1.context.flatten().len()],
        prompt1.context.flatten().as_slice(),
        "second prompt must keep first prompt context items as an exact prefix"
    );

    h.shutdown().expect("shutdown");
}

/// Sender-harness authorization must bind the bearer capability to every
/// security-sensitive field, especially the sender identity and watch-response
/// kind that the target harness will render into the recipient prompt.
#[test]
fn external_agent_message_auth_binds_sender_identity_and_kind() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let message_id = tau_proto::AgentMessageId::parse("msg-auth")
        .expect("test identifier must satisfy its grammar");
    h.peer_messaging.pending_external_message_auth.insert(
        message_id.clone(),
        crate::harness::PendingExternalAgentMessageAuth {
            capability: "secret-capability".to_owned(),
            sender_session_id: h.session_runtime.current_session_id.clone(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: test_session_id("target-session"),
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
                "recipient_agent",
            )),
            kind: tau_proto::AgentMessageKind::WatchResponse,
            message: "authorized body".to_owned(),
        },
    );

    let valid = tau_proto::ExternalAgentMessageAuthRequest {
        request_id: "auth-valid".to_owned(),
        message_id: message_id.clone(),
        capability: "secret-capability".to_owned(),
        sender_session_id: h.session_runtime.current_session_id.clone(),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: test_session_id("target-session"),
        recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
            "recipient_agent",
        )),
        kind: tau_proto::AgentMessageKind::WatchResponse,
        message: "authorized body".to_owned(),
    };
    let result = h.handle_external_agent_message_auth_request(valid.clone());
    assert!(result.authorized);
    assert_eq!(result.error, None);

    let forged_kind = tau_proto::ExternalAgentMessageAuthRequest {
        request_id: "auth-forged-kind".to_owned(),
        kind: tau_proto::AgentMessageKind::Message,
        ..valid.clone()
    };
    let result = h.handle_external_agent_message_auth_request(forged_kind);
    assert!(!result.authorized);
    assert!(result.error.expect("error").contains("does not match"));

    let forged_sender = tau_proto::ExternalAgentMessageAuthRequest {
        request_id: "auth-forged-sender".to_owned(),
        sender_id: crate::parse_agent_id("attacker"),
        ..valid.clone()
    };
    let result = h.handle_external_agent_message_auth_request(forged_sender);
    assert!(!result.authorized);
    assert!(result.error.expect("error").contains("does not match"));

    let forged_body = tau_proto::ExternalAgentMessageAuthRequest {
        request_id: "auth-forged-body".to_owned(),
        message: "altered body".to_owned(),
        ..valid
    };
    let result = h.handle_external_agent_message_auth_request(forged_body);
    assert!(!result.authorized);
    assert!(result.error.expect("error").contains("does not match"));

    h.shutdown().expect("shutdown");
}

/// Generic clients and extensions must not be able to forge external-message
/// RPCs. Even the narrow external-harness hello only enables the RPC envelope;
/// sender identity and kind still require a sender-issued capability.
#[test]
fn external_agent_message_rpc_requires_external_peer_hello() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    let request = tau_proto::ExternalAgentMessageRequest {
        request_id: "external-forged".to_owned(),
        message_id: tau_proto::AgentMessageId::parse("msg-external-forged")
            .expect("test identifier must satisfy its grammar"),
        capability: "cap-forged".to_owned(),
        sender_session_id: test_session_id("other-session"),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: test_session_id("s1"),
        recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
            &recipient_id,
        )),
        kind: tau_proto::AgentMessageKind::Message,
        message: "forged".to_owned(),
    };

    h.handle_client_message(
        &crate::test_connection_id("ui"),
        tau_proto::HarnessInputMessage::ExternalAgentMessage(request.clone()),
    )
    .expect("untrusted client request");
    h.handle_extension_message(
        &crate::test_connection_id("extension"),
        tau_proto::HarnessInputMessage::ExternalAgentMessage(request.clone()),
    )
    .expect("extension request");
    assert!(session_agent_message_received_events(&h).is_empty());

    h.handle_client_message(
        &crate::test_connection_id("ui"),
        tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("ordinary-ui"),
            client_kind: tau_proto::ClientKind::Ui,
            expected_session_id: None,
            capabilities: Default::default(),
        }),
    )
    .expect("ordinary hello");
    h.handle_client_message(
        &crate::test_connection_id("ui"),
        tau_proto::HarnessInputMessage::ExternalAgentMessage(request.clone()),
    )
    .expect("ordinary client request");
    assert!(session_agent_message_received_events(&h).is_empty());

    h.handle_client_message(
        &crate::test_connection_id("external"),
        tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name(
                crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME,
            ),
            client_kind: tau_proto::ClientKind::External,
            expected_session_id: None,
            capabilities: Default::default(),
        }),
    )
    .expect("external hello");
    h.handle_client_message(
        &crate::test_connection_id("external"),
        tau_proto::HarnessInputMessage::ExternalAgentMessage(request),
    )
    .expect("external request");
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// A real Unix-socket external client cannot deliver an external agent message
/// unless the claimed sender harness authenticates the message capability.
#[test]
fn external_agent_message_rpc_rejects_unauthenticated_socket_sender() {
    let td = TempDir::new().expect("tempdir");
    let sender_sp = td.path().join("sender-state");
    let target_sp = td.path().join("target-state");
    let mut sender = echo_harness(&sender_sp).expect("start sender");
    let mut target = echo_harness(&target_sp).expect("start target");
    let target_cid = ensure_test_user_agent(&mut target);
    let recipient_id = target
        .ensure_agent_id_for_agent(&target_cid)
        .expect("target agent id");
    target
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&target_cid)
        .expect("target conversation")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("socket-external-message-target"),
    };

    let socket_path = td.path().join("target.sock");
    let listener = path_std_os_unix_net::UnixListener::bind(&socket_path).expect("bind socket");
    let mut peer = tau_socket::SocketPeer::connect(&socket_path).expect("connect peer");
    let (stream, _) = listener.accept().expect("accept peer");
    target.accept_client(stream).expect("accept client");

    peer.send(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: crate::test_extension_name(crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME),
        client_kind: tau_proto::ClientKind::External,
        expected_session_id: None,
        capabilities: Default::default(),
    }))
    .expect("send external hello");
    peer.send(&tau_proto::HarnessInputMessage::ExternalAgentMessage(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "socket-external-ok".to_owned(),
            message_id: tau_proto::AgentMessageId::parse("socket-message-ok")
                .expect("test identifier must satisfy its grammar"),
            capability: "socket-capability".to_owned(),
            sender_session_id: sender.session_runtime.current_session_id.clone(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: target.session_runtime.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
                &recipient_id,
            )),
            kind: tau_proto::AgentMessageKind::Message,
            message: "hello over socket".to_owned(),
        },
    ))
    .expect("send external message");

    let deadline = Instant::now() + path_std_time::Duration::from_secs(5);
    let result = loop {
        let received = target
            .runtime_io
            .rx
            .recv_timeout(path_std_time::Duration::from_millis(20));
        match received.map(|event| target.expand_component_ingress_wake(event)) {
            Ok(HarnessEvent::FromConnection {
                connection_id,
                message,
                ..
            }) => {
                target
                    .handle_client_message(&connection_id, *message)
                    .expect("handle socket message");
            }
            Ok(HarnessEvent::Command(command)) => target
                .handle_harness_command(command)
                .expect("handle auth completion"),
            Ok(other) => target.log_event(&other),
            Err(path_std_sync_mpsc::RecvTimeoutError::Timeout) => {}
            Err(path_std_sync_mpsc::RecvTimeoutError::Disconnected) => {
                panic!("target harness event channel disconnected");
            }
        }
        match peer
            .recv_timeout(path_std_time::Duration::from_millis(20))
            .expect("receive rpc result")
        {
            tau_socket::SocketReceive::Message {
                message: tau_proto::HarnessOutputMessage::ExternalAgentMessageResult(result),
            } => break result,
            tau_socket::SocketReceive::Message { .. } => continue,
            tau_socket::SocketReceive::Timeout if Instant::now() < deadline => continue,
            tau_socket::SocketReceive::Timeout => panic!("timed out waiting for rpc result"),
            tau_socket::SocketReceive::Closed => panic!("socket closed before rpc result"),
        }
    };

    assert_eq!(result.request_id, "socket-external-ok");
    assert_eq!(
        result.failure,
        Some(tau_proto::ExternalAgentMessageFailure::Rejected)
    );
    assert!(session_agent_message_received_events(&target).is_empty());

    target.shutdown().expect("shutdown target");
    sender.shutdown().expect("shutdown sender");
}

/// Two real harness event loops complete callback correlation and delegated
/// auto-start over separate Unix sockets, promote the newly started endpoint
/// to canonical `active` navigation, and acknowledge only after the receive
/// projection commits.
#[test]
fn external_agent_message_two_harness_live_success_commits_before_ack() {
    let td = TempDir::new().expect("tempdir");
    let mut sender = echo_harness(td.path().join("sender-state")).expect("start sender");
    let mut target = echo_harness(td.path().join("target-state")).expect("start target");
    let sender_cid = ensure_test_user_agent(&mut sender);
    let sender_id = crate::parse_agent_id(
        sender
            .ensure_agent_id_for_agent(&sender_cid)
            .expect("sender id"),
    );
    configure_inter_session_receivers(&mut target, &[("engineer", true)]);
    let ui_frames =
        connect_test_client(&mut target, "peer-auto-start-ui", tau_proto::ClientKind::Ui);
    target
        .runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("peer-auto-start-ui"),
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STATS_UPDATED,
            )],
        )
        .expect("subscribe UI to agent stats");
    let message_id: tau_proto::AgentMessageId =
        tau_proto::AgentMessageId::parse("two-harness-message")
            .expect("test identifier must satisfy its grammar");
    let request = tau_proto::ExternalAgentMessageRequest {
        request_id: "two-harness-request".to_owned(),
        message_id: message_id.clone(),
        capability: "two-harness-capability".to_owned(),
        sender_session_id: sender.session_runtime.current_session_id.clone(),
        sender_id: sender_id.clone(),
        recipient_session_id: target.session_runtime.current_session_id.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: "hello between harnesses".to_owned(),
    };
    sender.peer_messaging.pending_external_message_auth.insert(
        message_id,
        crate::harness::PendingExternalAgentMessageAuth {
            capability: request.capability.clone(),
            sender_session_id: request.sender_session_id.clone(),
            sender_id,
            recipient_session_id: request.recipient_session_id.clone(),
            recipient: request.recipient.clone(),
            kind: request.kind,
            message: request.message.clone(),
        },
    );

    let sender_path = td.path().join("sender-daemon");
    let sender_listener =
        path_std_os_unix_net::UnixListener::bind(crate::runtime_dir::socket_path(&sender_path))
            .expect("bind sender");
    let sender_tx = sender.runtime_io.tx.clone();
    let accept_sender = std::thread::spawn(move || {
        let (stream, _) = sender_listener.accept().expect("accept callback");
        sender_tx
            .send(HarnessEvent::NewClient(stream))
            .expect("forward callback");
    });
    let _sender_registration = crate::runtime_dir::register_test_session_harness(
        sender.session_runtime.current_session_id.as_str(),
        sender_path,
    );

    let target_socket = td.path().join("target.sock");
    let target_listener =
        path_std_os_unix_net::UnixListener::bind(&target_socket).expect("bind target");
    let mut peer = tau_socket::SocketPeer::connect(&target_socket).expect("connect target");
    let (stream, _) = target_listener.accept().expect("accept target");
    target
        .accept_client(stream)
        .expect("register target client");
    peer.send(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: crate::test_extension_name(crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME),
        client_kind: tau_proto::ClientKind::External,
        expected_session_id: None,
        capabilities: Default::default(),
    }))
    .expect("target hello");
    peer.send(&tau_proto::HarnessInputMessage::ExternalAgentMessage(
        request,
    ))
    .expect("target request");

    let deadline = Instant::now() + Duration::from_secs(5);
    let result = loop {
        for harness in [&mut target, &mut sender] {
            let received = harness
                .runtime_io
                .rx
                .recv_timeout(Duration::from_millis(10));
            match received.map(|event| harness.expand_component_ingress_wake(event)) {
                Ok(HarnessEvent::FromConnection {
                    connection_id,
                    message,
                    ..
                }) => {
                    harness
                        .handle_client_message(&connection_id, *message)
                        .expect("handle peer frame");
                }
                Ok(HarnessEvent::NewClient(stream)) => {
                    harness
                        .accept_client(stream)
                        .expect("accept callback client");
                }
                Ok(HarnessEvent::Command(command)) => harness
                    .handle_harness_command(command)
                    .expect("handle peer command"),
                Ok(other) => harness.log_event(&other),
                Err(path_std_sync_mpsc::RecvTimeoutError::Timeout) => {}
                Err(path_std_sync_mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("harness event loop disconnected")
                }
            }
        }
        match peer
            .recv_timeout(Duration::from_millis(10))
            .expect("peer result")
        {
            tau_socket::SocketReceive::Message {
                message: tau_proto::HarnessOutputMessage::ExternalAgentMessageResult(result),
            } => break result,
            tau_socket::SocketReceive::Message { .. } | tau_socket::SocketReceive::Timeout
                if Instant::now() < deadline => {}
            tau_socket::SocketReceive::Timeout => panic!("timed out waiting for target result"),
            tau_socket::SocketReceive::Closed => panic!("target closed before result"),
            tau_socket::SocketReceive::Message { .. } => {}
        }
    };

    assert_eq!(result.failure, None);
    let recipient_id = result.recipient_id.expect("resolved auto-start recipient");
    assert!(result.started);
    assert!(
        target
            .agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(recipient_id.as_str())
    );
    assert_eq!(target.agent_runtime.agent_registry.agents.len(), 1);
    assert_eq!(
        target
            .agent_runtime
            .agent_registry
            .navigation_modes
            .get(&recipient_id),
        Some(&tau_proto::AgentNavigationMode::Active)
    );
    let recipient_cid = target
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(recipient_id.as_str())
        .expect("auto-started recipient route");
    let stats = target
        .agent_stats_snapshot(recipient_cid)
        .expect("auto-started recipient stats");
    assert_eq!(
        stats.navigation_mode,
        tau_proto::AgentNavigationMode::Active
    );
    assert_eq!(stats.runtime_state, tau_proto::AgentRuntimeState::Running);
    assert!(
        ui_frames
            .lock()
            .expect("UI frame mutex")
            .iter()
            .any(|frame| {
                matches!(
                    peel_inner_event(&frame.frame),
                    Some(Event::AgentStatsUpdated(stats))
                        if stats.agent_id == recipient_id
                            && stats.navigation_mode == tau_proto::AgentNavigationMode::Active
                )
            })
    );
    assert_eq!(durable_agent_message_received_events(&target).len(), 1);
    accept_sender.join().expect("sender accept thread");
    target.shutdown().expect("shutdown target");
    sender.shutdown().expect("shutdown sender");
}

/// Receiver-side sender authentication must run off the central harness loop.
/// A request whose claimed sender cannot authenticate should enqueue a helper
/// and return promptly instead of blocking every other harness event until the
/// socket/auth timeout path completes.
#[test]
fn external_agent_message_authentication_starts_without_blocking_client_handler() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");

    h.handle_client_message(
        &crate::test_connection_id("external"),
        tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name(
                crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME,
            ),
            client_kind: tau_proto::ClientKind::External,
            expected_session_id: None,
            capabilities: Default::default(),
        }),
    )
    .expect("external hello");

    let started = Instant::now();
    h.handle_client_message(
        &crate::test_connection_id("external"),
        tau_proto::HarnessInputMessage::ExternalAgentMessage(
            tau_proto::ExternalAgentMessageRequest {
                request_id: "external-nonblocking-auth".to_owned(),
                message_id: tau_proto::AgentMessageId::parse("msg-external-nonblocking-auth")
                    .expect("test message id must satisfy its grammar"),
                capability: "cap-nonblocking".to_owned(),
                sender_session_id: test_session_id("missing-sender-session"),
                sender_id: crate::parse_agent_id("sender_agent"),
                recipient_session_id: h.session_runtime.current_session_id.clone(),
                recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
                    &recipient_id,
                )),
                kind: tau_proto::AgentMessageKind::Message,
                message: "hello".to_owned(),
            },
        ),
    )
    .expect("external request");
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "external auth should be delegated off-loop"
    );
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Sender-side result waiting must outlast receiver-side sender authentication.
/// The target cannot send `external_agent_message_result` until its auth helper
/// finishes, so equal deadlines can make the sender close the socket exactly
/// when the receiver is ready to report an auth timeout or late success.
#[test]
fn external_agent_message_result_timeout_outlasts_auth_timeout() {
    let auth_timeout = path_crate_harness::subagents_tool::EXTERNAL_AGENT_MESSAGE_AUTH_TIMEOUT;
    let result_timeout = path_crate_harness::subagents_tool::EXTERNAL_AGENT_MESSAGE_RESULT_TIMEOUT;

    assert!(
        auth_timeout < result_timeout,
        "result timeout must leave room for target-side auth completion"
    );
}

/// Sender-side external message projections should represent confirmed
/// delivery, not a failed lookup or target-side rejection.
#[test]
fn external_message_send_failure_does_not_publish_sent_projection() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: tau_proto::ToolCallId = "external-message-call".into();
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), cid.clone());

    h.publish_external_agent_message_from_agent(
        &cid,
        test_session_id("missing-session"),
        tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id("recipient_agent")),
        "hello".to_owned(),
        tau_proto::AgentMessageKind::Message,
        Some(
            path_crate_harness::subagents_tool::ExternalMessageToolCompletion {
                conversation_id: cid.clone(),
                session_generation: h.session_runtime.current_session_generation,
                call_id: call_id.clone(),
                tool_name: ToolName::new(path_crate_harness::subagents_tool::MESSAGE_TOOL_NAME),
                tool_type: tau_proto::ToolType::Function,
                details: CborValue::Null,
            },
        ),
    )
    .expect("start external send");
    assert!(session_agent_message_sent_events(&h).is_empty());
    assert_eq!(h.peer_messaging.pending_external_message_auth.len(), 1);

    let command = loop {
        match h
            .runtime_io
            .rx
            .recv_timeout(path_std_time::Duration::from_secs(5))
            .expect("completion command")
        {
            HarnessEvent::Command(command) => break command,
            other => h.log_event(&other),
        }
    };
    h.handle_harness_command(command)
        .expect("handle completion");

    assert!(session_agent_message_sent_events(&h).is_empty());
    assert!(h.peer_messaging.pending_external_message_auth.is_empty());
    assert!(
        event_log_events(&h)
            .into_iter()
            .any(|event| { matches!(event, Event::ToolError(error) if error.call_id == call_id) })
    );

    h.shutdown().expect("shutdown");
}

/// A reachable target without an inter-session receiver must show the caller a
/// configuration action rather than the generic unavailable-session failure.
#[test]
fn external_message_no_receiver_failure_is_actionable_to_caller() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: tau_proto::ToolCallId = "external-message-no-receiver".into();
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), cid.clone());

    h.handle_harness_command(
        path_crate_event::HarnessCommand::ExternalMessageToolCompleted(Box::new(
            crate::event::ExternalMessageToolCompletedCommand {
                _permit: None,
                conversation_id: cid,
                session_generation: h.session_runtime.current_session_generation,
                call_id: call_id.clone(),
                tool_name: ToolName::new(path_crate_harness::subagents_tool::MESSAGE_TOOL_NAME),
                tool_type: tau_proto::ToolType::Function,
                result: Err(path_crate_event::ExternalMessageDeliveryError::Target(
                    tau_proto::ExternalAgentMessageFailure::NoInterSessionReceiver,
                )),
                details: CborValue::Null,
                auth_message_id: tau_proto::AgentMessageId::parse("no-receiver-message")
                    .expect("test identifier must satisfy its grammar"),
                publish_sent: true,
                sender_id: crate::parse_agent_id("sender_agent"),
                recipient_session_id: test_session_id("reachable-session"),
                kind: tau_proto::AgentMessageKind::Message,
                message: "hello".to_owned(),
            },
        )),
    )
    .expect("handle completion");

    let error = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::ToolError(error) if error.call_id == call_id => Some(error.message),
            _ => None,
        })
        .expect("caller-visible tool error");
    assert_eq!(
        error,
        "target live; no receiver; set `inter_session_receiver`"
    );
    assert!(session_agent_message_sent_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Cross-session message successes hide bare-recipient reuse and auto-start
/// mechanics while retaining the unambiguous delivery status and recipient.
#[test]
fn external_message_success_results_hide_bare_recipient_start_state() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let completions = [
        (
            "external-message-reused-call",
            "delivered-reused-message",
            "reused_recipient",
            false,
        ),
        (
            "external-message-auto-started-call",
            "delivered-auto-started-message",
            "auto_started_recipient",
            true,
        ),
    ];

    for (call_id, message_id, recipient_id, started) in completions {
        let call_id: tau_proto::ToolCallId = call_id.into();
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert(call_id.clone(), cid.clone());
        h.handle_harness_command(
            path_crate_event::HarnessCommand::ExternalMessageToolCompleted(Box::new(
                crate::event::ExternalMessageToolCompletedCommand {
                    _permit: None,
                    conversation_id: cid.clone(),
                    session_generation: h.session_runtime.current_session_generation,
                    call_id,
                    tool_name: ToolName::new(path_crate_harness::subagents_tool::MESSAGE_TOOL_NAME),
                    tool_type: tau_proto::ToolType::Function,
                    result: Ok((crate::parse_agent_id(recipient_id), started)),
                    details: CborValue::Null,
                    auth_message_id: tau_proto::AgentMessageId::parse(message_id)
                        .expect("test identifier must satisfy its grammar"),
                    publish_sent: true,
                    sender_id: crate::parse_agent_id("sender_agent"),
                    recipient_session_id: test_session_id("other-session"),
                    kind: tau_proto::AgentMessageKind::Message,
                    message: "delivered".to_owned(),
                },
            )),
        )
        .expect("handle completion");
    }

    let results = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolResult(result) => Some((result.call_id, result.result)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(session_agent_message_sent_events(&h).len(), 2);
    assert_eq!(
        results,
        vec![
            (
                "external-message-reused-call".into(),
                CborValue::Map(vec![
                    (
                        CborValue::Text("status".to_owned()),
                        CborValue::Text(
                            "Message committed: delivered-reused-message; recipient was live; response not guaranteed"
                                .to_owned(),
                        ),
                    ),
                    (
                        CborValue::Text("message_id".to_owned()),
                        CborValue::Text("delivered-reused-message".to_owned()),
                    ),
                    (
                        CborValue::Text("recipient".to_owned()),
                        CborValue::Text("other-session/reused_recipient".to_owned()),
                    ),
                ]),
            ),
            (
                "external-message-auto-started-call".into(),
                CborValue::Map(vec![
                    (
                        CborValue::Text("status".to_owned()),
                        CborValue::Text(
                            "Message committed: delivered-auto-started-message; recipient was live; response not guaranteed"
                                .to_owned(),
                        ),
                    ),
                    (
                        CborValue::Text("message_id".to_owned()),
                        CborValue::Text("delivered-auto-started-message".to_owned()),
                    ),
                    (
                        CborValue::Text("recipient".to_owned()),
                        CborValue::Text("other-session/auto_started_recipient".to_owned()),
                    ),
                ]),
            ),
        ],
        "reused and auto-created bare recipients must produce the same unambiguous success shape"
    );

    h.shutdown().expect("shutdown");
}

/// Bare routing applies established idle-first fairness across receiver roles
/// from different configured groups.
#[test]
fn bare_peer_route_selects_one_idle_entrypoint_endpoint() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let reviewer_role = h.config.available_roles["engineer"].clone();
    h.config
        .available_roles
        .insert("cross-group-reviewer".to_owned(), reviewer_role);
    configure_inter_session_receivers(
        &mut h,
        &[("engineer", false), ("cross-group-reviewer", false)],
    );
    let busy = ensure_test_user_agent(&mut h);
    let busy_id = h.ensure_agent_id_for_agent(&busy).expect("busy id");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&busy)
        .expect("busy agent")
        .turn
        .published_runtime_state = tau_proto::AgentRuntimeState::Running;
    let idle = h.create_durable_user_agent(test_session_id("s1"), "cross-group-reviewer");
    let idle_id = h.ensure_agent_id_for_agent(&idle).expect("idle id");
    let request = tau_proto::ExternalAgentMessageRequest {
        request_id: "bare-select".to_owned(),
        message_id: tau_proto::AgentMessageId::parse("bare-select-message")
            .expect("test identifier must satisfy its grammar"),
        capability: "test-only".to_owned(),
        sender_session_id: test_session_id("sender-session"),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: h.session_runtime.current_session_id.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: "hello peer".to_owned(),
    };

    let result = h.handle_external_agent_message_request_without_auth_for_test(request);

    assert_eq!(result.failure, None);
    assert_eq!(
        result.recipient_id.as_ref().map(ToString::to_string),
        Some(idle_id.to_string())
    );
    assert!(!result.started);
    let received = durable_agent_message_received_events(&h);
    assert_eq!(received.len(), 1);
    assert_ne!(received[0].recipient_id.as_str(), busy_id.as_str());
}

/// Loaded endpoints whose creation role no longer resolves to an available
/// provider model are ineligible instead of silently accepting unserviceable
/// peer work.
#[test]
fn bare_peer_route_rejects_endpoint_after_role_model_becomes_unavailable() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    configure_inter_session_receivers(&mut h, &[("engineer", false)]);
    h.create_durable_user_agent(test_session_id("s1"), "engineer");
    h.provider_runtime.model_info.clear();
    let request = tau_proto::ExternalAgentMessageRequest {
        request_id: "model-revoked".to_owned(),
        message_id: tau_proto::AgentMessageId::parse("model-revoked-message")
            .expect("test identifier must satisfy its grammar"),
        capability: "test-only".to_owned(),
        sender_session_id: test_session_id("sender-session"),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: h.session_runtime.current_session_id.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: "hello peer".to_owned(),
    };

    let result = h.handle_external_agent_message_request_without_auth_for_test(request);

    assert_eq!(
        result.failure,
        Some(tau_proto::ExternalAgentMessageFailure::NoInterSessionReceiver)
    );
    assert!(durable_agent_message_received_events(&h).is_empty());
}

/// The separate auto-start role grant creates one ordinary role-backed endpoint
/// when no eligible endpoint exists and uses the peer input as its first
/// prompt.
#[test]
fn bare_peer_route_starts_explicit_role_without_remote_ancestry() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    configure_inter_session_receivers(&mut h, &[("engineer", true)]);
    let agents_before = h.agent_runtime.agent_registry.agents.len();
    let request = tau_proto::ExternalAgentMessageRequest {
        request_id: "auto-start".to_owned(),
        message_id: tau_proto::AgentMessageId::parse("auto-start-message")
            .expect("test identifier must satisfy its grammar"),
        capability: "test-only".to_owned(),
        sender_session_id: test_session_id("sender-session"),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: h.session_runtime.current_session_id.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: "hello peer".to_owned(),
    };

    let result = h.handle_external_agent_message_request_without_auth_for_test(request);

    assert_eq!(result.failure, None);
    assert!(result.started);
    assert_eq!(
        h.agent_runtime.agent_registry.agents.len(),
        agents_before + 1
    );
    let recipient_id = result.recipient_id.expect("resolved recipient");
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(recipient_id.as_str())
        .expect("auto-started route");
    let agent = h
        .agent_runtime
        .agent_registry
        .agents
        .get(cid)
        .expect("auto-started agent");
    assert_eq!(agent.identity.role.as_deref(), Some("engineer"));
    assert_eq!(agent.identity.parent_agent_id, None);
    assert_eq!(agent.identity.parent_tool_call_id, None);
    assert!(
        !h.is_non_tool_extension_query(cid),
        "peer endpoint must retain ordinary tools and loaded-agent lifecycle"
    );
    assert_eq!(durable_agent_message_received_events(&h).len(), 1);
    let records = h
        .session_runtime
        .agent_store
        .agent_events(&recipient_id)
        .expect("auto-started agent records");
    let received_observation = records
        .iter()
        .find(|record| matches!(record.event, Event::AgentMessageReceived(_)))
        .map(|record| record.observation_id)
        .expect("durable receive observation");
    let activations = records
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentActivationQueued(tau_proto::AgentActivationQueued {
                    kind: tau_proto::ActivationKind::AgentMessage,
                    source_observation: Some(source),
                    source_call: None,
                }) if *source == received_observation
            )
        })
        .count();
    assert_eq!(
        activations, 1,
        "precreation buffering retains one activation linked to its receive"
    );
}

/// Multiple usable auto-start grants choose the first configured role without
/// warning or hash-map iteration.
#[test]
fn bare_peer_auto_start_uses_first_configured_candidate() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let role = h.config.available_roles["engineer"].clone();
    h.config
        .available_roles
        .insert("preferred-receiver".to_owned(), role.clone());
    h.config
        .available_roles
        .insert("fallback-receiver".to_owned(), role);
    configure_inter_session_receivers(
        &mut h,
        &[("preferred-receiver", true), ("fallback-receiver", true)],
    );

    let result = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "ordered-auto-start".to_owned(),
            message_id: tau_proto::AgentMessageId::parse("ordered-auto-start-message")
                .expect("test identifier must satisfy its grammar"),
            capability: "test-only".to_owned(),
            sender_session_id: test_session_id("sender-session"),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: h.session_runtime.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "choose deterministically".to_owned(),
        },
    );

    assert_eq!(result.failure, None);
    let recipient = result.recipient_id.expect("auto-started recipient");
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(recipient.as_str())
        .expect("recipient route");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[cid]
            .identity
            .role
            .as_deref(),
        Some("preferred-receiver")
    );
}

/// Auto-start walks past a role pruned from runtime availability and a receiver
/// whose explicitly configured model is unavailable, then selects the next
/// usable grant.
#[test]
fn bare_peer_auto_start_skips_unavailable_role_model() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let mut unavailable = h.config.available_roles["engineer"].clone();
    unavailable.model = Some("missing/model".parse().expect("model id"));
    h.config
        .available_roles
        .insert("unavailable-receiver".to_owned(), unavailable);
    configure_inter_session_receivers(
        &mut h,
        &[
            ("required-skill-pruned-receiver", true),
            ("unavailable-receiver", true),
            ("engineer", true),
        ],
    );

    let result = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "skip-unavailable-auto-start".to_owned(),
            message_id: tau_proto::AgentMessageId::parse("skip-unavailable-auto-start-message")
                .expect("test message id must satisfy its grammar"),
            capability: "test-only".to_owned(),
            sender_session_id: test_session_id("sender-session"),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: h.session_runtime.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "fall back".to_owned(),
        },
    );

    assert_eq!(result.failure, None);
    let recipient = result.recipient_id.expect("fallback recipient");
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(recipient.as_str())
        .expect("recipient route");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[cid]
            .identity
            .role
            .as_deref(),
        Some("engineer")
    );
}

/// The explicit durable peer-purpose marker survives a cold resume before any
/// provider response and preserves ordinary tool-capable loaded-agent
/// lifecycle.
#[test]
fn peer_auto_start_lifecycle_marker_survives_cold_resume() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let recipient_id = {
        let mut h = echo_harness(&sp).expect("start");
        let _interceptor = connect_test_tool(&mut h, "peer-marker-interceptor");
        h.handle_extension_event(
            "peer-marker-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register metadata interceptor");
        configure_inter_session_receivers(&mut h, &[("engineer", true)]);
        let result = h.handle_external_agent_message_request_without_auth_for_test(
            tau_proto::ExternalAgentMessageRequest {
                request_id: "restore-peer".to_owned(),
                message_id: tau_proto::AgentMessageId::parse("restore-peer-message")
                    .expect("test identifier must satisfy its grammar"),
                capability: "test-only".to_owned(),
                sender_session_id: test_session_id("sender-session"),
                sender_id: crate::parse_agent_id("sender_agent"),
                recipient_session_id: h.session_runtime.current_session_id.clone(),
                recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
                kind: tau_proto::AgentMessageKind::Message,
                message: "persist purpose before response".to_owned(),
            },
        );
        let recipient_id = result.recipient_id.expect("auto-start recipient");
        assert!(
            h.runtime_io.publication.pending_intercept.is_some(),
            "creation fact is ordered"
        );
        h.handle_extension_event(
            "peer-marker-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("attempt to drop protected creation fact");
        assert!(h.runtime_io.publication.pending_intercept.is_none());
        h.shutdown().expect("shutdown");
        recipient_id
    };

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(recipient_id.as_str())
        .cloned()
        .expect("restored peer endpoint");

    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .peer_entrypoint_endpoint
    );
    assert!(!h.is_non_tool_extension_query(&cid));
    assert!(
        !Harness::agent_uses_non_tool_prompt_surface(&h.agent_runtime.agent_registry.agents[&cid]),
        "restored peer endpoint retains ordinary tool authority"
    );
    assert!(Harness::is_peer_entrypoint_agent(
        &h.agent_runtime.agent_registry.agents[&cid]
    ));
    h.shutdown().expect("shutdown resumed harness");
}

/// Receive commit rejects a nominal auto-start reservation whose immutable
/// creation fact lacks the reserved peer-purpose marker, as after persistence
/// failure of that creation fact.
#[test]
fn peer_auto_start_requires_durable_marked_creation_before_receive_commit() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = h.create_durable_user_agent(test_session_id("s1"), "engineer");
    let agent_id = crate::parse_agent_id(
        h.ensure_agent_id_for_agent(&cid)
            .expect("ordinary agent id"),
    );
    h.peer_messaging
        .uncommitted_peer_auto_starts
        .insert(agent_id.clone());

    assert!(
        !h.peer_auto_start_creation_committed(&agent_id),
        "ordinary creation without reserved marker cannot establish auto-start"
    );
}

/// Concurrent live requests coalesce through the endpoint inserted by the first
/// auto-start, and a busy eligible endpoint is reused rather than fanning out.
#[test]
fn bare_peer_auto_start_is_live_single_flight_and_reuses_busy_agent() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    configure_inter_session_receivers(&mut h, &[("engineer", true)]);
    let target_session = h.session_runtime.current_session_id.clone();
    let request = |suffix: &str| tau_proto::ExternalAgentMessageRequest {
        request_id: format!("single-flight-{suffix}"),
        message_id: tau_proto::AgentMessageId::parse(format!("single-flight-message-{suffix}"))
            .expect("test message id must satisfy its grammar"),
        capability: "test-only".to_owned(),
        sender_session_id: test_session_id("sender-session"),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: target_session.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: format!("hello {suffix}"),
    };

    let first = h.handle_external_agent_message_request_without_auth_for_test(request("one"));
    let recipient = first.recipient_id.clone().expect("first recipient");
    assert!(first.started);
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(recipient.as_str())
        .cloned()
        .expect("auto-started route");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("auto-started agent")
        .turn
        .published_runtime_state = tau_proto::AgentRuntimeState::Running;

    let second = h.handle_external_agent_message_request_without_auth_for_test(request("two"));

    assert_eq!(second.failure, None);
    assert_eq!(second.recipient_id, Some(recipient));
    assert!(!second.started);
    assert_eq!(h.agent_runtime.agent_registry.agents.len(), 1);
}

/// Queue admission counts uncheckpointed peer wakes before any auto-start or
/// receive projection can create additional model work.
#[test]
fn peer_input_queue_limit_rejects_before_auto_start_spend() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    configure_inter_session_receivers(&mut h, &[("engineer", true)]);
    let cid = h.create_durable_user_agent(test_session_id("s1"), "engineer");
    for index in 0..32 {
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .dispatch
            .pending_message_wakes
            .push_back(crate::agent::PendingMessageWake {
                source: path_crate_agent::PendingMessageWakeSource::AgentMessageReceived {
                    durable_event_seq: tau_core::PersistedAgentEventSeq::new(index),
                    activation_class:
                        path_crate_agent::AgentMessageActivationClass::OrdinaryAgentInput,
                    peer_admission_bytes: Some(1),
                },
                node_id: None,
                activation_observation: None,
                source_observation: None,
                delivery_schedule: None,
            });
    }
    let agents_before = h.agent_runtime.agent_registry.agents.len();
    let request = tau_proto::ExternalAgentMessageRequest {
        request_id: "queue-full".to_owned(),
        message_id: tau_proto::AgentMessageId::parse("queue-full-message")
            .expect("test identifier must satisfy its grammar"),
        capability: "test-only".to_owned(),
        sender_session_id: test_session_id("sender-session"),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: h.session_runtime.current_session_id.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: "rejected".to_owned(),
    };

    let result = h.handle_external_agent_message_request_without_auth_for_test(request);

    assert_eq!(
        result.failure,
        Some(tau_proto::ExternalAgentMessageFailure::Rejected)
    );
    assert_eq!(h.agent_runtime.agent_registry.agents.len(), agents_before);
    assert!(durable_agent_message_received_events(&h).is_empty());
}

/// Accepted start placeholders participate in the same peer queue count and
/// byte accounting as already loaded endpoints.
#[test]
fn pending_endpoint_peer_queue_enforces_count_and_byte_bounds() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    configure_inter_session_receivers(&mut h, &[("engineer", false)]);
    let _interceptor = connect_test_tool(&mut h, "pending-endpoint-interceptor");
    h.handle_extension_event(
        "pending-endpoint-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register creation interceptor");
    let pending_id = h
        .enqueue_internal_start_agent_request_without_draining(StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            query_id: "pending-peer-endpoint".to_owned(),
            instruction: "ordinary pending task".to_owned(),
            role: Some("engineer".to_owned()),
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
            parent_agent: None,
        })
        .expect("reserve pending endpoint");
    h.drain_pending_start_agent_requests()
        .expect("commit acceptance");
    let pending = h
        .agent_runtime
        .agent_registry
        .start_coordinator
        .operations
        .values_mut()
        .find(|operation| operation.pending.agent_id == pending_id)
        .map(|operation| &mut operation.pending)
        .expect("accepted endpoint");
    pending
        .pending_agent_message_wakes
        .extend((0..32).map(|index| crate::agent::PendingMessageWake {
            source: path_crate_agent::PendingMessageWakeSource::AgentMessageReceived {
                durable_event_seq: tau_core::PersistedAgentEventSeq::new(index),
                activation_class: path_crate_agent::AgentMessageActivationClass::OrdinaryAgentInput,
                peer_admission_bytes: Some(1),
            },
            node_id: None,
            activation_observation: None,
            source_observation: None,
            delivery_schedule: None,
        }));
    let recipient = crate::parse_agent_id(&pending_id);
    assert_eq!(
        h.admit_peer_input(&recipient, 1)
            .expect_err("count boundary rejects"),
        "peer input queue is full; retry later"
    );

    let pending = h
        .agent_runtime
        .agent_registry
        .start_coordinator
        .operations
        .values_mut()
        .find(|operation| operation.pending.agent_id == pending_id)
        .map(|operation| &mut operation.pending)
        .expect("accepted endpoint");
    pending.pending_agent_message_wakes.clear();
    pending
        .pending_agent_message_wakes
        .extend((0..4).map(|index| crate::agent::PendingMessageWake {
            source: path_crate_agent::PendingMessageWakeSource::AgentMessageReceived {
                durable_event_seq: tau_core::PersistedAgentEventSeq::new(index),
                activation_class: path_crate_agent::AgentMessageActivationClass::OrdinaryAgentInput,
                peer_admission_bytes: Some(64 * 1024),
            },
            node_id: None,
            activation_observation: None,
            source_observation: None,
            delivery_schedule: None,
        }));
    assert_eq!(
        h.admit_peer_input(&recipient, 1)
            .expect_err("byte boundary rejects"),
        "peer input queue is full; retry later"
    );
}

/// A live endpoint cannot accept an unbounded burst even when each prior prompt
/// has already left its queue.
#[test]
fn peer_input_rate_limit_bounds_live_burst() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    configure_inter_session_receivers(&mut h, &[("engineer", false)]);
    let cid = h.create_durable_user_agent(test_session_id("s1"), "engineer");
    let recipient =
        crate::parse_agent_id(h.ensure_agent_id_for_agent(&cid).expect("public agent id"));
    for index in 0..60 {
        h.admit_peer_input(&recipient, 7)
            .unwrap_or_else(|error| panic!("request {index}: {error}"));
    }
    let rejected = h
        .admit_peer_input(&recipient, 8)
        .expect_err("61st input rejected");

    assert_eq!(rejected, "peer input rate limit reached; retry later");
}

/// Sender authentication binds the typed route authority so an exact request
/// cannot substitute for a pending bare route with the same body and ids.
#[test]
fn external_message_auth_rejects_bare_exact_capability_substitution() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let sender = ensure_test_user_agent(&mut h);
    let sender_id = crate::parse_agent_id(
        h.ensure_agent_id_for_agent(&sender)
            .expect("sender public id"),
    );
    let message_id: tau_proto::AgentMessageId =
        tau_proto::AgentMessageId::parse("typed-auth-message")
            .expect("test identifier must satisfy its grammar");
    h.peer_messaging.pending_external_message_auth.insert(
        message_id.clone(),
        crate::harness::PendingExternalAgentMessageAuth {
            capability: "typed-capability".to_owned(),
            sender_session_id: h.session_runtime.current_session_id.clone(),
            sender_id: sender_id.clone(),
            recipient_session_id: test_session_id("target-session"),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "same body".to_owned(),
        },
    );
    let result =
        h.handle_external_agent_message_auth_request(tau_proto::ExternalAgentMessageAuthRequest {
            request_id: "typed-auth".to_owned(),
            message_id,
            capability: "typed-capability".to_owned(),
            sender_session_id: h.session_runtime.current_session_id.clone(),
            sender_id,
            recipient_session_id: test_session_id("target-session"),
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
                "known_agent",
            )),
            kind: tau_proto::AgentMessageKind::Message,
            message: "same body".to_owned(),
        });
    assert!(!result.authorized);
}

/// A cold resume restores historical session membership even though an
/// unloaded agent has no live route. Exact local classification and external
/// target validation must continue to report that known id as stopped.
#[test]
fn cold_resume_reports_historically_unloaded_message_recipient_as_stopped() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let recipient_id = {
        let mut h = echo_harness(&sp).expect("start");
        let stopped_cid: AgentId = crate::parse_agent_id("cold-stopped-recipient");
        h.agent_runtime.agent_registry.agents.insert(
            stopped_cid.clone(),
            Agent::new(
                stopped_cid.clone(),
                1,
                test_session_id("s1"),
                tau_proto::PromptOriginator::User,
                None,
                None,
            ),
        );
        let recipient_id = h.ensure_agent_id_for_agent(&stopped_cid).expect("agent id");
        h.remove_agent(&stopped_cid);
        h.shutdown().expect("shutdown");
        recipient_id
    };

    let mut resumed =
        echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
            .expect("cold resume");
    assert_eq!(
        resumed.agent_message_recipient_status(&recipient_id),
        crate::harness::AgentMessageRecipientStatus::Stopped
    );
    let result = resumed.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "external-cold-stopped".to_owned(),
            message_id: tau_proto::AgentMessageId::parse("msg-external-cold-stopped")
                .expect("test identifier must satisfy its grammar"),
            capability: "cap-cold-stopped".to_owned(),
            sender_session_id: test_session_id("other-session"),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: test_session_id("s1"),
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
                &recipient_id,
            )),
            kind: tau_proto::AgentMessageKind::Message,
            message: "hello".to_owned(),
        },
    );
    assert_eq!(
        result.failure,
        Some(tau_proto::ExternalAgentMessageFailure::RecipientStopped)
    );
    assert!(session_agent_message_received_events(&resumed).is_empty());
    resumed.shutdown().expect("shutdown resumed");
}

/// A sender's real inline `message` completion may wake a recipient's sole
/// activating-input `wait` while the sender projection remains parked in a
/// non-idle publication batch. Both durable terminals must schedule exactly one
/// successor after the batch, leave no deferred dispatch ownership behind, and
/// preserve later activation dispatchability. The sender's successor retains
/// the original tool call and compact result without replaying the routed body
/// as an assistant response.
#[test]
fn nested_message_and_input_wait_drain_both_publish_idle_dispatches() {
    const BODY: &str = "nested publication wake";
    const MESSAGE_CALL_ID: &str = "message-nested-wake";
    const WAIT_CALL_ID: &str = "wait-nested-wake";

    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let sender_cid = ensure_test_user_agent(&mut h);
    let role = h.config.selected_role.clone();
    let recipient_cid =
        h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let sender_id = durable_agent_id_for_conversation(&h, &sender_cid);
    let recipient_id = durable_agent_id_for_conversation(&h, &recipient_cid);
    finish_test_agent_context_wait(&mut h, &recipient_id);

    h.dispatch_prompt_for_agent(
        &recipient_cid,
        PendingPrompt::user("wait for activating input".to_owned()),
    )
    .expect("dispatch recipient setup");
    let recipient_setup = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(provider_input_wait_response(
        &recipient_setup,
        WAIT_CALL_ID,
        10,
    ))
    .expect("register recipient input wait");
    assert!(h.input_wait_pending_for(&recipient_cid));
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&recipient_cid]
            .turn
            .turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));

    h.dispatch_prompt_for_agent(
        &sender_cid,
        PendingPrompt::user("send the nested wake".to_owned()),
    )
    .expect("dispatch sender setup");
    let sender_setup = read_nth_prompt_created(&h, 1);
    let checkpoints_before = event_log_count(&h, |event| {
        matches!(event, Event::AgentInferenceDispatchStarted(_))
    });
    let interceptor = connect_test_tool(&mut h, "nested-message-interceptor");
    h.handle_extension_event(
        "nested-message-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_SENT,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register sender projection interceptor");

    h.handle_provider_response_finished(provider_tool_response(
        &sender_setup,
        MESSAGE_CALL_ID,
        path_crate_harness::subagents_tool::MESSAGE_TOOL_NAME,
        message_tool_call(MESSAGE_CALL_ID, recipient_id.as_str(), BODY).arguments,
    ))
    .expect("run sender message tool");
    assert!(h.runtime_io.publication.pending_intercept.is_some());
    assert!(!h.runtime_io.publication.deferred.is_empty());
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    h.handle_extension_event(
        "nested-message-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit nested publication batch");
    h.process_notification_delivery_deadlines_at(Instant::now() + Duration::from_millis(60_000));
    drop(interceptor);

    assert!(!h.input_wait_pending_for(&recipient_cid));
    assert!(h.runtime_io.publication.pending_intercept.is_none());
    assert!(h.runtime_io.publication.deferred.is_empty());
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    let checkpoints = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), checkpoints_before + 2);
    assert_eq!(
        checkpoints
            .iter()
            .skip(checkpoints_before)
            .filter(|checkpoint| checkpoint.agent_id == sender_id)
            .count(),
        1
    );
    assert_eq!(
        checkpoints
            .iter()
            .skip(checkpoints_before)
            .filter(|checkpoint| checkpoint.agent_id == recipient_id)
            .count(),
        1
    );

    let sender_successor_id = h.agent_runtime.agent_registry.agents[&sender_cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("sender successor");
    let recipient_successor_id = h.agent_runtime.agent_registry.agents[&recipient_cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("recipient successor");
    let sender_successor = read_prompt_created(&h, &sender_successor_id);
    let recipient_successor = read_prompt_created(&h, &recipient_successor_id);
    let sender_context = sender_successor.context.flatten();
    assert!(sender_context.iter().any(|item| {
        matches!(
            item,
            ContextItem::ToolCall(call)
                if call.call_id.as_str() == MESSAGE_CALL_ID
                    && cbor_map_text(&call.arguments, "message") == Some(BODY)
        )
    }));
    assert_eq!(
        sender_context
            .iter()
            .filter(|item| tool_result_id(item) == Some(MESSAGE_CALL_ID))
            .count(),
        1
    );
    assert!(
        !sender_context.iter().any(|item| {
            matches!(
                item,
                ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content,
                    ..
                }) if content.iter().any(|part| matches!(
                    part,
                    ContentPart::Text { text }
            | ContentPart::SyntheticCompactionSummary { text }
            | ContentPart::HarnessInternalText { text }
                        if text.contains(BODY)
                ))
            )
        }),
        "the sender must not replay the routed body as assistant output"
    );
    let recipient_context = recipient_successor.context.flatten();
    assert_eq!(
        recipient_context
            .iter()
            .filter(|item| {
                text_part(item).is_some_and(|text| {
                    text.contains(&format!(
                        "<tau_internal>You have received a message from {sender_id}"
                    )) && text.contains(BODY)
                })
            })
            .count(),
        1
    );
    assert_eq!(
        recipient_context
            .iter()
            .filter(|item| tool_result_id(item) == Some(WAIT_CALL_ID))
            .count(),
        1
    );
    let recipient_events = h
        .session_runtime
        .agent_store
        .agent_events(recipient_id.as_str())
        .expect("recipient journal");
    assert_eq!(
        recipient_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentMessageReceived(message)
                    if message.sender_id == sender_id && message.message == BODY
            ))
            .count(),
        1
    );
    assert_eq!(
        recipient_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ProviderToolResult(result)
                    if result.call_id.as_str() == WAIT_CALL_ID
                        && result.result == CborValue::Map(vec![(
                            CborValue::Text("input_available".to_owned()),
                            CborValue::Bool(true),
                        )])
            ))
            .count(),
        1
    );
    let sender_events = h
        .session_runtime
        .agent_store
        .agent_events(sender_id.as_str())
        .expect("sender journal");
    assert_eq!(
        sender_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentMessageSent(message)
                    if message.recipient
                        == tau_proto::AgentMessageRecipient::Agent {
                            agent_id: recipient_id.clone(),
                        }
                        && message.message == BODY
            ))
            .count(),
        1
    );
    let sent_message_id = sender_events
        .iter()
        .find_map(|record| match &record.event {
            Event::AgentMessageSent(message)
                if message.recipient
                    == tau_proto::AgentMessageRecipient::Agent {
                        agent_id: recipient_id.clone(),
                    }
                    && message.message == BODY =>
            {
                Some(message.message_id.to_string())
            }
            _ => None,
        })
        .expect("sender message projection");
    assert_eq!(
        sender_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ProviderToolResult(result)
                    if result.call_id.as_str() == MESSAGE_CALL_ID
                        && result.result == CborValue::Text(format!(
                            "Message committed: {sent_message_id}; recipient was live; response not guaranteed"
                        ))
            ))
            .count(),
        1
    );

    h.handle_provider_response_finished(provider_text_response(
        &recipient_successor.agent_prompt_id,
        recipient_id.clone(),
        "recipient successor complete",
    ))
    .expect("finish recipient successor");
    h.handle_provider_response_finished(provider_text_response(
        &sender_successor.agent_prompt_id,
        sender_id,
        "sender successor complete",
    ))
    .expect("finish sender successor");
    let recipient_prompts_before_timer = event_log_count(&h, |event| {
        matches!(
            event,
            Event::AgentPromptCreated(prompt) if prompt.agent_id == recipient_id
        )
    });
    assert_eq!(
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            recipient_id.as_str(),
            PendingPrompt::internal("later timer activation".to_owned()),
        )
        .expect("submit later timer activation"),
        PromptSubmission::Dispatched
    );
    assert_eq!(
        event_log_count(&h, |event| {
            matches!(
                event,
                Event::AgentPromptCreated(prompt) if prompt.agent_id == recipient_id
            )
        }),
        recipient_prompts_before_timer + 1
    );
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    h.shutdown().expect("shutdown");
}

/// A local message received while its target is blocked on an unrelated exact
/// wait must not let that target's deferred activation block the sender's
/// terminal or the next parallel message. This reproduces the publication cut
/// that stranded a coordinator after the first recipient committed the message
/// but before the sender committed its tool result.
#[test]
fn local_message_to_exact_waiter_releases_sender_and_parallel_successor() {
    const FIRST_BODY: &str = "first exact-wait message";
    const SECOND_BODY: &str = "second parallel message";
    const BACKGROUND_CALL_ID: &str = "exact-wait-background";
    const WAIT_CALL_ID: &str = "exact-wait-message";
    const STATUS_CALL_ID: &str = "status-before-messages";
    const FIRST_MESSAGE_CALL_ID: &str = "message-exact-wait";
    const SECOND_MESSAGE_CALL_ID: &str = "message-parallel-successor";

    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = echo_harness(&state).expect("start");
    h.config.selected_model = Some("test/model".into());
    h.config
        .accepted_harness_settings
        .notification_delivery
        .agent_message =
        NotificationDeliveryPolicy::from_millis(0, 0, 0).expect("immediate test policy");
    let sender_cid = ensure_test_user_agent(&mut h);
    let role = h.config.selected_role.clone();
    let first_recipient_cid =
        h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let second_recipient_cid =
        h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let sender_id = durable_agent_id_for_conversation(&h, &sender_cid);
    let first_recipient_id = durable_agent_id_for_conversation(&h, &first_recipient_cid);
    let second_recipient_id = durable_agent_id_for_conversation(&h, &second_recipient_cid);
    finish_test_agent_context_wait(&mut h, &first_recipient_id);
    finish_test_agent_context_wait(&mut h, &second_recipient_id);

    let _tool_events = connect_test_tool(&mut h, "exact-wait-message-tool");
    h.tool_routing.registry.register(
        &crate::test_connection_id("exact-wait-message-tool"),
        instant_background_test_tool_spec("slow_exact_wait"),
    );
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &first_recipient_cid,
        BACKGROUND_CALL_ID,
        "slow_exact_wait",
    );
    h.dispatch_prompt_for_agent(
        &first_recipient_cid,
        PendingPrompt::user("wait for exact background call".to_owned()),
    )
    .expect("dispatch exact-wait setup");
    let recipient_setup_id = h.agent_runtime.agent_registry.agents[&first_recipient_cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("exact-wait setup prompt");
    let recipient_setup = read_prompt_created(&h, &recipient_setup_id);
    h.handle_provider_response_finished(provider_tool_response(
        &recipient_setup,
        WAIT_CALL_ID,
        "wait",
        CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(BACKGROUND_CALL_ID.to_owned()),
        )]),
    ))
    .expect("register recipient exact wait");
    assert_eq!(tool_result_count(&h, WAIT_CALL_ID), 0);

    h.dispatch_prompt_for_agent(
        &sender_cid,
        PendingPrompt::user("wait for timer".to_owned()),
    )
    .expect("dispatch sender wait");
    let sender_wait_prompt_id = h.agent_runtime.agent_registry.agents[&sender_cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("sender wait prompt");
    let sender_wait_prompt = read_prompt_created(&h, &sender_wait_prompt_id);
    h.handle_provider_response_finished(provider_input_wait_response(
        &sender_wait_prompt,
        "sender-activating-wait",
        60,
    ))
    .expect("open sender activating wait");
    assert_eq!(
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            sender_id.as_str(),
            PendingPrompt::internal("timer activation".to_owned()),
        )
        .expect("activate sender wait"),
        PromptSubmission::Queued
    );
    let sender_setup_id = h.agent_runtime.agent_registry.agents[&sender_cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("sender setup prompt");
    let sender_setup = read_prompt_created(&h, &sender_setup_id);
    let mut response = provider_tool_response(
        &sender_setup,
        STATUS_CALL_ID,
        "status",
        CborValue::Map(vec![
            (
                CborValue::Text("state".to_owned()),
                CborValue::Text("working".to_owned()),
            ),
            (
                CborValue::Text("task_name".to_owned()),
                CborValue::Text("testing parallel message continuation".to_owned()),
            ),
        ]),
    );
    let first_call = message_tool_call(
        FIRST_MESSAGE_CALL_ID,
        first_recipient_id.as_str(),
        FIRST_BODY,
    );
    response
        .output_items
        .push(ContextItem::ToolCall(ToolCallItem {
            call_id: first_call.id,
            name: first_call.name,
            tool_type: first_call.tool_type,
            arguments: first_call.arguments,
            raw_arguments_json: None,
            responses_envelope: None,
        }));
    let second_call = message_tool_call(
        SECOND_MESSAGE_CALL_ID,
        second_recipient_id.as_str(),
        SECOND_BODY,
    );
    response
        .output_items
        .push(ContextItem::ToolCall(ToolCallItem {
            call_id: second_call.id,
            name: second_call.name,
            tool_type: second_call.tool_type,
            arguments: second_call.arguments,
            raw_arguments_json: None,
            responses_envelope: None,
        }));
    let checkpoints_before = event_log_count(&h, |event| {
        matches!(event, Event::AgentInferenceDispatchStarted(_))
    });
    let sender_checkpoints_before = event_log_events(&h)
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::AgentInferenceDispatchStarted(started) if started.agent_id == sender_id
            )
        })
        .count();
    let recipient_checkpoints_before =
        [&first_recipient_id, &second_recipient_id].map(|agent_id| {
            event_log_events(&h)
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    Event::AgentInferenceDispatchStarted(started) if &started.agent_id == agent_id
                )
            })
            .count()
        });
    let interceptor = connect_test_tool(&mut h, "exact-wait-message-interceptor");
    h.handle_extension_event(
        "exact-wait-message-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_SENT,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register sender projection interceptor");
    h.handle_provider_response_finished(response)
        .expect("run parallel local messages");
    assert!(h.runtime_io.publication.pending_intercept.is_some());
    let terminal_interceptor = connect_test_tool(&mut h, "message-terminal-interceptor");
    h.handle_extension_event(
        "message-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_TOOL_RESULT,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register sender terminal interceptor");
    h.handle_extension_event(
        "exact-wait-message-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit first recipient and park sender terminal");
    h.handle_extension_event(
        "message-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit exact-wait interruption and park sender terminal");
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "message-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("reject first sender terminal append");
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentMessageReceived(message)
                    if message.recipient_id == first_recipient_id
                        && message.message == FIRST_BODY
            ))
            .count(),
        1,
        "the recipient side effect committed before sender-terminal retry"
    );
    reject_next_semantic_admission(&h);
    h.retry_pending_agent_publish_completion(&sender_cid);
    assert!(
        h.cancel_remaining_tool_calls(
            &sender_cid,
            vec![ToolCallId::from(FIRST_MESSAGE_CALL_ID)],
            BackgroundCompletionPromptMode::QueueAndAdvance,
        ),
        "retained terminal remains the foreground cancellation owner"
    );
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::ToolCancelled(cancelled)
            if cancelled.call_id.as_str() == FIRST_MESSAGE_CALL_ID
    )));
    h.retry_pending_agent_publish_completion(&sender_cid);
    assert!(
        h.runtime_io.publication.pending_intercept.is_some(),
        "retained sender terminal retry must release the second message"
    );
    h.handle_extension_event(
        "exact-wait-message-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit second local-message batch");
    h.handle_extension_event(
        "message-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit second sender terminal");
    drop(interceptor);
    drop(terminal_interceptor);

    assert_eq!(
        tool_result_count(&h, WAIT_CALL_ID),
        1,
        "activating message interrupts the exact wait exactly once"
    );
    for (recipient_id, body) in [
        (&first_recipient_id, FIRST_BODY),
        (&second_recipient_id, SECOND_BODY),
    ] {
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::AgentMessageReceived(message)
                        if &message.recipient_id == recipient_id && message.message == body
                ))
                .count(),
            1,
            "missing receive for {body}"
        );
    }
    for call_id in [FIRST_MESSAGE_CALL_ID, SECOND_MESSAGE_CALL_ID] {
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::ProviderToolResult(result) if result.call_id.as_str() == call_id
                ))
                .count(),
            1,
            "missing sender terminal for {call_id}"
        );
    }
    let events = event_log_events(&h);
    let position = |predicate: &dyn Fn(&Event) -> bool| {
        events
            .iter()
            .position(predicate)
            .expect("incident-defining event")
    };
    let first_receive = position(&|event| {
        matches!(
            event,
            Event::AgentMessageReceived(message)
                if message.recipient_id == first_recipient_id && message.message == FIRST_BODY
        )
    });
    let first_terminal = position(&|event| {
        matches!(
            event,
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == FIRST_MESSAGE_CALL_ID
        )
    });
    let second_receive = position(&|event| {
        matches!(
            event,
            Event::AgentMessageReceived(message)
                if message.recipient_id == second_recipient_id && message.message == SECOND_BODY
        )
    });
    let second_terminal = position(&|event| {
        matches!(
            event,
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == SECOND_MESSAGE_CALL_ID
        )
    });
    assert!(
        first_receive < first_terminal
            && first_terminal < second_receive
            && second_receive < second_terminal,
        "parallel calls must retain the incident-defining serial order"
    );
    assert_eq!(
        event_log_count(&h, |event| {
            matches!(event, Event::AgentInferenceDispatchStarted(_))
        }),
        checkpoints_before + 3,
        "the sender and both activated recipients continue exactly once"
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentInferenceDispatchStarted(started) if started.agent_id == sender_id
            ))
            .count(),
        sender_checkpoints_before + 1,
        "the sender must continue exactly once after both terminals"
    );
    for (agent_id, before) in [
        (&first_recipient_id, recipient_checkpoints_before[0]),
        (&second_recipient_id, recipient_checkpoints_before[1]),
    ] {
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::AgentInferenceDispatchStarted(started)
                        if &started.agent_id == agent_id
                ))
                .count(),
            before + 1,
            "each activated recipient must continue exactly once"
        );
    }
    assert!(h.runtime_io.publication.pending_intercept.is_none());
    assert!(h.runtime_io.publication.deferred.is_empty());
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    let sender_successor = h.agent_runtime.agent_registry.agents[&sender_cid]
        .dispatch
        .in_flight_prompt
        .as_ref()
        .expect("sender successor");
    assert_eq!(
        read_prompt_created(&h, sender_successor).agent_id,
        sender_id.clone()
    );
    h.shutdown().expect("shutdown");
    drop(h);
    wait_for_session_unlock(&state, "s1");
    let resumed =
        echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
            .expect("cold resume");
    let first_recipient_events = resumed
        .session_runtime
        .agent_store
        .agent_events(first_recipient_id.as_str())
        .expect("restored first recipient");
    assert_eq!(
        first_recipient_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentMessageReceived(message)
                    if message.sender_id == sender_id && message.message == FIRST_BODY
            ))
            .count(),
        1,
        "cold replay must not resend the already committed message"
    );
    let sender_events = resumed
        .session_runtime
        .agent_store
        .agent_events(sender_id.as_str())
        .expect("restored sender");
    assert_eq!(
        sender_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentMessageSent(message) if message.message == FIRST_BODY
            ))
            .count(),
        1
    );
}

/// Explicit navigation may leave an owed wake dormant, but a sibling checkpoint
/// must never acknowledge it; reselecting its branch makes it runnable again.
#[test]
fn agent_message_wake_stays_dormant_off_branch_until_reselected() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = echo_harness(&state).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("branch-message-busy"),
    };

    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("branch-message")
                .expect("test identifier must satisfy its grammar"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "branch-owned input".to_owned(),
        }),
    );
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::parse("branch-bridge").expect("publisher"),
            tau_proto::MessageAgentTarget::new(recipient_id.as_str()),
            tau_proto::MessageFactId::new("branch-raw-message"),
            tau_proto::MessageParty {
                stable_id: "external".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            None,
            "branch-owned raw input",
        )),
    );
    let wake_node = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .pending_message_wakes
        .back()
        .and_then(|wake| wake.node_id)
        .expect("materialized message wake");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_message_wakes
            .len(),
        2
    );
    h.set_agent_turn_state(&cid, AgentTurnState::Idle);
    let checkpoints_before = event_log_events(&h)
        .iter()
        .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
        .count();
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: crate::parse_agent_id(&recipient_id),
            head: tau_proto::AgentHead::Root,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id: crate::parse_agent_id(&recipient_id),
            text: "selected sibling".to_owned(),
            inference_activation: false,
            message_class: tau_proto::PromptMessageClass::Internal,
        }),
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        checkpoints_before,
        "off-branch wake must remain dormant"
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_message_wakes
            .len(),
        2
    );
    h.shutdown().expect("shutdown before cold replay");
    drop(h);
    wait_for_session_unlock(&state, "s1");

    let mut h = echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
        .expect("cold resume");
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(&recipient_id)
        .cloned()
        .expect("restored durable agent route");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_message_wakes
            .len(),
        2,
        "typed and raw occurrences remain dormant on the incomparable branch; records={:?}",
        h.session_runtime
            .agent_store
            .agent_events(&recipient_id)
            .expect("agent records")
            .iter()
            .map(|record| record.event.name())
            .collect::<Vec<_>>()
    );
    let checkpoints_before = event_log_events(&h)
        .iter()
        .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
        .count();
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: crate::parse_agent_id(&recipient_id),
            head: tau_proto::AgentHead::Node(wake_node),
        }),
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        checkpoints_before + 1
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_message_wakes
            .is_empty()
    );
}

/// Watch responses retain their typed canonical wrapper without a generated
/// payload prompt.
#[test]
fn agent_watch_response_uses_distinct_canonical_projection() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("conversation")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("sp-watch-target"),
    };

    h.publish_agent_watch_response_from_agent(
        &cid,
        recipient_id.to_string(),
        "done <response>&</response> payload >".to_owned(),
    )
    .expect("watch response");

    assert!(session_agent_message_sent_events(&h).is_empty());
    let received = session_agent_message_received_events(&h);
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].kind, tau_proto::AgentMessageKind::WatchResponse);

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("conversation");
    assert!(conv.dispatch.pending_prompts.is_empty());
    assert_eq!(conv.dispatch.pending_message_wakes.len(), 1);
    let context = crate::prompt::assemble_prompt_context_from(
        h.session_runtime
            .agent_store
            .agent(&recipient_id)
            .expect("watcher tree"),
        conv.identity.head,
    )
    .context
    .flatten();
    let text = context
        .iter()
        .filter_map(text_part)
        .find(|text| text.contains("Watched agent"))
        .expect("canonical watch response");
    assert!(text.contains(&format!(
        "<tau_internal>Watched agent {recipient_id} emitted a response"
    )));
    assert!(text.contains("<response>\ndone <response>&&lt;/response&gt; payload >\n</response>"));
    assert!(!text.contains("You have received a message"));
    assert!(!text.contains("<message>"));

    h.shutdown().expect("shutdown");
}

/// An eligible user prompt without watchers must move its text into the durable
/// submission without allocating a second copy for a fanout that cannot occur.
#[test]
fn user_prompt_without_watchers_does_not_clone_text_for_watch_fanout() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let watched_cid = ensure_test_user_agent(&mut h);
    let watched_id = h.agent_runtime.agent_registry.agents[&watched_cid]
        .identity
        .agent_id
        .clone()
        .expect("watched agent id");

    h.reset_watch_prompt_text_clone_count_for_test();
    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: false,
            session_id: h.session_runtime.current_session_id.clone(),
            text: "unwatched prompt".to_owned(),
            agent_id: tau_proto::AgentId::parse(&watched_id).expect("watched id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("prompt submitted");

    assert_eq!(
        h.watch_prompt_text_clone_count_for_test(),
        0,
        "the empty reverse index must avoid the fanout-text allocation"
    );
    assert!(
        !session_agent_message_received_events(&h)
            .iter()
            .any(|message| message.kind == tau_proto::AgentMessageKind::WatchPrompt),
        "no reverse-index entry must produce no watch delivery"
    );
    h.shutdown().expect("shutdown");
}

/// A watcher must be told when the watched agent accepts a direct user prompt,
/// otherwise the watched agent's next response can look like an unsolicited
/// reply to the watcher instead of a response to fresh user input.
#[test]
fn user_prompt_to_watched_agent_notifies_watchers_with_prompt_markup() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = h.agent_runtime.agent_registry.agents[&watched_cid]
        .identity
        .agent_id
        .clone()
        .expect("watched agent id");
    let watcher_id = h.agent_runtime.agent_registry.agents[&watcher_cid]
        .identity
        .agent_id
        .clone()
        .expect("watcher agent id");
    finish_test_agent_context_wait(
        &mut h,
        &tau_proto::AgentId::parse(&watcher_id).expect("watcher id"),
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watcher_cid)
        .expect("watcher conversation")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("sp-watcher-busy"),
    };
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    h.reset_watch_prompt_text_clone_count_for_test();
    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: false,
            session_id: h.session_runtime.current_session_id.clone(),
            text: "please continue <now>&</now> >".to_owned(),
            agent_id: tau_proto::AgentId::parse(&watched_id).expect("watched id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("prompt submitted");

    assert_eq!(
        h.watch_prompt_text_clone_count_for_test(),
        1,
        "a nonempty reverse index copies the prompt exactly once before fanout"
    );
    assert!(session_agent_message_sent_events(&h).is_empty());
    let received: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| message.kind == tau_proto::AgentMessageKind::WatchPrompt)
        .collect();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].kind, tau_proto::AgentMessageKind::WatchPrompt);
    assert_eq!(received[0].sender_id, crate::parse_agent_id(&watched_id));
    assert_eq!(received[0].recipient_id, crate::parse_agent_id(&watcher_id));

    let watcher = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&watcher_cid)
        .expect("watcher conversation");
    assert!(watcher.dispatch.pending_prompts.is_empty());
    assert_eq!(
        watcher.dispatch.pending_message_wakes.len(),
        1,
        "only user-prompt content activates after turn-state producer removal"
    );
    let context = crate::prompt::assemble_prompt_context_from(
        h.session_runtime
            .agent_store
            .agent(&watcher_id)
            .expect("watcher tree"),
        watcher.identity.head,
    )
    .context
    .flatten();
    let text = context
        .iter()
        .filter_map(text_part)
        .find(|text| text.contains("received a user prompt"))
        .expect("canonical prompt notification");
    assert!(text.contains(&format!(
        "<tau_internal>Watched agent {watched_id} received a user prompt"
    )));
    assert!(text.contains("<prompt>\nplease continue <now>&</now> >\n</prompt>"));
    assert!(!text.contains("finished its turn"));
    assert!(!text.contains("<response>"));

    h.shutdown().expect("shutdown");
}

/// Prompt fanout must retain sorted watcher order, exact payload identity, and
/// error cleanup when a stale reverse-index entry shares a delivery batch.
#[test]
fn user_prompt_watch_fanout_orders_exact_payloads_and_prunes_failed_delivery() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let second_watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = h.agent_runtime.agent_registry.agents[&watched_cid]
        .identity
        .agent_id
        .clone()
        .expect("watched agent id");
    let watcher_ids = [watcher_cid, second_watcher_cid].map(|cid| {
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .clone()
            .expect("watcher agent id")
    });
    for watcher_id in &watcher_ids {
        h.set_agent_watch(
            watcher_id,
            &watched_id,
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        );
    }
    h.set_agent_watch(
        "missing-watcher",
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    let prompt_text = "exact <watch>& prompt".to_owned();
    h.reset_watch_prompt_text_clone_count_for_test();
    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: false,
            session_id: h.session_runtime.current_session_id.clone(),
            text: prompt_text.clone(),
            agent_id: tau_proto::AgentId::parse(&watched_id).expect("watched id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("prompt submitted");

    assert_eq!(h.watch_prompt_text_clone_count_for_test(), 1);
    let received: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| message.kind == tau_proto::AgentMessageKind::WatchPrompt)
        .collect();
    let mut expected_recipients = watcher_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    expected_recipients.sort();
    assert_eq!(
        received
            .iter()
            .map(|message| message.recipient_id.to_string())
            .collect::<Vec<_>>(),
        expected_recipients,
        "the BTree reverse index preserves watcher delivery order"
    );
    for message in &received {
        assert_eq!(message.sender_id, crate::parse_agent_id(&watched_id));
        assert_eq!(message.message, prompt_text);
    }
    assert!(
        !h.watchers_for_agent(&watched_id)
            .iter()
            .any(|watcher_id| watcher_id == "missing-watcher"),
        "a failed delivery still prunes only its stale relation"
    );
    assert_eq!(
        h.watchers_for_agent(watched_id.as_str()),
        expected_recipients
    );
    h.shutdown().expect("shutdown");
}

/// Internal prompts delivered to a watched agent, including background tool
/// completion notices and steering prompts, must not be reflected as
/// `agent_watch` prompt notifications. Only user-visible prompts are watchable
/// context for the watcher.
#[test]
fn internal_prompt_to_watched_agent_does_not_notify_watchers() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = h.agent_runtime.agent_registry.agents[&watched_cid]
        .identity
        .agent_id
        .clone()
        .expect("watched agent id");
    let watcher_id = h.agent_runtime.agent_registry.agents[&watcher_cid]
        .identity
        .agent_id
        .clone()
        .expect("watcher agent id");
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: false,
            session_id: h.session_runtime.current_session_id.clone(),
            text: background_completion_prompt(&ToolCallId::from("call-1")),
            agent_id: tau_proto::AgentId::parse(&watched_id).expect("watched id"),
            message_class: tau_proto::PromptMessageClass::Internal,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("internal prompt submitted");

    assert!(
        !session_agent_message_received_events(&h)
            .iter()
            .any(|message| message.kind == tau_proto::AgentMessageKind::WatchPrompt),
        "internal prompts to watched agents must not be forwarded to watchers"
    );

    h.shutdown().expect("shutdown");
}

/// A queued prompt must not notify watchers until it becomes the watched
/// agent's active turn. Otherwise the watcher can receive "prompt arrived" and
/// then the previous turn's response, which is exactly the ordering ambiguity
/// watch prompt notifications are meant to remove.
#[test]
fn queued_user_prompt_notifies_watchers_when_dispatched_not_when_queued() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = h.agent_runtime.agent_registry.agents[&watched_cid]
        .identity
        .agent_id
        .clone()
        .expect("watched agent id");
    let watcher_id = h.agent_runtime.agent_registry.agents[&watcher_cid]
        .identity
        .agent_id
        .clone()
        .expect("watcher agent id");
    finish_test_agent_context_wait(
        &mut h,
        &tau_proto::AgentId::parse(&watcher_id).expect("watcher id"),
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watched_cid)
        .expect("watched conversation")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("sp-watched-current"),
    };
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watcher_cid)
        .expect("watcher conversation")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("sp-watcher-busy"),
    };
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let watched_agent_id = tau_proto::AgentId::parse(&watched_id).expect("watched id");
    h.agent_runtime.agent_registry.navigation_modes.insert(
        watched_agent_id.clone(),
        tau_proto::AgentNavigationMode::Suspended,
    );

    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: false,
            session_id: h.session_runtime.current_session_id.clone(),
            text: "queued follow-up".to_owned(),
            agent_id: watched_agent_id.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("prompt queued");

    assert!(
        !session_agent_message_received_events(&h)
            .iter()
            .any(|message| message.kind == tau_proto::AgentMessageKind::WatchPrompt),
        "queued prompt must not notify watchers before it becomes active"
    );
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&watched_agent_id),
        Some(&tau_proto::AgentNavigationMode::Active),
        "accepted queued UI input resumes immediately"
    );
    h.write_loaded_agent_navigation_mode(
        &watched_agent_id,
        tau_proto::AgentNavigationMode::Suspended,
    )
    .expect("explicit suspend after queue admission");

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watched_cid)
        .expect("watched conversation")
        .turn
        .turn_state = AgentTurnState::Idle;
    h.try_advance_queue();

    let received: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| message.kind == tau_proto::AgentMessageKind::WatchPrompt)
        .collect();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].kind, tau_proto::AgentMessageKind::WatchPrompt);
    assert_eq!(received[0].message, "queued follow-up");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&watched_agent_id),
        Some(&tau_proto::AgentNavigationMode::Suspended),
        "later queue dispatch or steer must not reapply the implicit write"
    );

    h.shutdown().expect("shutdown");
}

/// External clients and extensions must not forge message projection events;
/// only the harness-owned message tool may publish them.
#[test]
fn inbound_agent_message_events_are_ignored() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let forged_sent = Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: tau_proto::AgentMessageId::parse("test-message")
            .expect("test identifier must satisfy its grammar"),
        sender_id: crate::parse_agent_id("attacker"),
        recipient: tau_proto::AgentMessageRecipient::Agent {
            agent_id: crate::parse_agent_id("victim"),
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: "forged".to_owned(),
    });
    let forged_received = Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("test-message-received")
            .expect("test identifier must satisfy its grammar"),
        sender_id: crate::parse_agent_id("attacker"),
        sender_session_id: Some(test_session_id("other-session")),
        recipient_id: crate::parse_agent_id("victim"),
        kind: tau_proto::AgentMessageKind::Message,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        message: "forged received".to_owned(),
    });
    for forged in [forged_sent, forged_received] {
        h.handle_client_event_inner(&crate::test_connection_id("ui"), forged.clone())
            .expect("client event");
        h.handle_extension_event_inner(&crate::test_connection_id("extension"), forged.clone())
            .expect("extension event");
        h.handle_extension_message(
            &crate::test_connection_id("extension"),
            TestMessage::Emit(tau_proto::Emit {
                event: Box::new(forged),
                persist: true,
            }),
        )
        .expect("extension emit");
    }

    assert!(session_agent_message_sent_events(&h).is_empty());
    assert!(session_agent_message_received_events(&h).is_empty());
    assert!(durable_agent_message_sent_events(&h).is_empty());
    assert!(durable_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Ensures full prompt rendering uses only harness-owned discovered AGENTS.md
/// state when AGENTS inclusion is enabled.
#[test]
fn rendered_prompt_with_seeded_agents_md_includes_synthetic_agents_message() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    seed_render_prompt_role(&mut h);
    h.prompt_coordination
        .context_discovery
        .agents_files
        .push(DiscoveredAgentsFile {
            source_id: crate::test_connection_id("shell"),
            file_path: td.path().join("AGENTS.md"),
            content: "seeded AGENTS instructions\n".to_owned(),
        });

    let result = request_rendered_prompt(&mut h, "debug-role", true);
    let prompt = result.prompt.expect("rendered prompt");

    assert_eq!(result.error, None);
    assert!(prompt.contains("<message role=\"user\" synthetic=\"true\" source=\"AGENTS.md\">"));
    assert_eq!(
        prompt
            .lines()
            .filter(|line| *line == "# agents.md files")
            .count(),
        1
    );
    assert!(prompt.contains("# agents.md files\n\n<AGENTS_FILE"));
    assert!(prompt.contains("<AGENTS_FILE path="));
    assert!(prompt.contains("seeded AGENTS instructions"));
}

/// The authoritative harness boundary must accept DAG shapes and repeated
/// enables, reject every closing edge atomically, let disables repair paths,
/// and serialize reciprocal requests so exactly the first direction wins.
#[test]
fn agent_watch_enforces_acyclic_topology_without_rejection_mutation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cids = [
        ensure_test_user_agent(&mut h),
        h.create_durable_user_agent(
            h.session_runtime.current_session_id.clone(),
            &h.config.selected_role.clone(),
        ),
        h.create_durable_user_agent(
            h.session_runtime.current_session_id.clone(),
            &h.config.selected_role.clone(),
        ),
        h.create_durable_user_agent(
            h.session_runtime.current_session_id.clone(),
            &h.config.selected_role.clone(),
        ),
    ];
    let ids: Vec<_> = cids
        .iter()
        .map(|cid| durable_agent_id_for_conversation(&h, cid).to_string())
        .collect();
    let [a, b, c, d] = ids.as_slice() else {
        unreachable!("four ids")
    };
    let enable = tau_proto::AgentWatchUpdateCause::AgentWatchEnable;
    let disable = tau_proto::AgentWatchUpdateCause::AgentWatchDisable;

    h.try_set_agent_watch(a, b, true, enable).expect("A -> B");
    let subscription = h.agent_runtime.agent_watch.subscriptions[&(a.clone(), b.clone())].clone();
    h.try_set_agent_watch(a, b, true, enable)
        .expect("re-enable existing edge");
    assert_eq!(
        h.agent_runtime.agent_watch.subscriptions[&(a.clone(), b.clone())],
        subscription,
        "re-enable retains subscription identity"
    );
    assert_eq!(
        h.try_set_agent_watch(b, a, true, enable),
        Err(format!("agent watch would create a cycle: `{b}` -> `{a}`")),
        "the first serialized reciprocal direction wins"
    );
    h.try_set_agent_watch(a, c, true, enable).expect("A -> C");
    h.try_set_agent_watch(b, d, true, enable).expect("B -> D");
    h.try_set_agent_watch(c, d, true, enable).expect("C -> D");
    h.try_set_agent_watch(b, c, true, enable)
        .expect("non-closing diamond cross-edge");

    let maps_before = (
        h.agent_runtime.agent_watch.forward.clone(),
        h.agent_runtime.agent_watch.reverse.clone(),
        h.agent_runtime.agent_watch.subscriptions.clone(),
        h.agent_runtime.agent_watch.provider_status.clone(),
        h.agent_runtime.agent_watch.provider_deliveries.clone(),
    );
    let events_before = event_log_events(&h).len();
    assert_eq!(
        h.try_set_agent_watch(d, a, true, enable),
        Err(format!("agent watch would create a cycle: `{d}` -> `{a}`"))
    );
    assert_eq!(
        (
            h.agent_runtime.agent_watch.forward.clone(),
            h.agent_runtime.agent_watch.reverse.clone(),
            h.agent_runtime.agent_watch.subscriptions.clone(),
            h.agent_runtime.agent_watch.provider_status.clone(),
            h.agent_runtime.agent_watch.provider_deliveries.clone(),
        ),
        maps_before,
        "cycle rejection must leave all watch authority unchanged"
    );
    assert_eq!(
        event_log_events(&h).len(),
        events_before,
        "cycle rejection must publish no topology or initial-state event"
    );
    assert_eq!(
        h.try_set_agent_watch(a, a, true, enable),
        Err("`agent_id` must identify another agent".to_owned())
    );

    h.try_set_agent_watch(a, c, false, disable)
        .expect("disable bypasses cycle analysis");
    h.try_set_agent_watch(b, c, false, disable)
        .expect("disable remaining path");
    h.try_set_agent_watch(c, a, true, enable)
        .expect("removed path permits formerly closing direction");
    assert_eq!(
        h.try_set_agent_watch(a, c, true, enable),
        Err(format!("agent watch would create a cycle: `{a}` -> `{c}`"))
    );

    // A deliberately invariant-violating fixture proves disable never consults
    // reachability and can dismantle malformed topology.
    h.agent_runtime
        .agent_watch
        .forward
        .entry(b.clone())
        .or_default()
        .insert(a.clone());
    h.agent_runtime
        .agent_watch
        .reverse
        .entry(a.clone())
        .or_default()
        .insert(b.clone());
    h.try_set_agent_watch(b, a, false, disable)
        .expect("disable repairs malformed cycle");
    assert!(
        !h.agent_runtime
            .agent_watch
            .forward
            .get(b)
            .is_some_and(|ids| ids.contains(a))
    );

    h.remove_agent(&cids[3]);
    h.agent_runtime
        .agent_watch
        .forward
        .entry(d.clone())
        .or_default()
        .insert(a.clone());
    let stopped_before = (
        h.agent_runtime.agent_watch.forward.clone(),
        h.agent_runtime.agent_watch.reverse.clone(),
        h.agent_runtime.agent_watch.subscriptions.clone(),
        h.agent_runtime.agent_watch.provider_status.clone(),
        h.agent_runtime.agent_watch.provider_deliveries.clone(),
    );
    let stopped_events_before = event_log_events(&h).len();
    assert_eq!(
        h.try_set_agent_watch(a, d, true, enable),
        Err(format!("agent is not live: `{d}`")),
        "Live-target classification must precede closing-path analysis"
    );
    assert_eq!(
        (
            h.agent_runtime.agent_watch.forward.clone(),
            h.agent_runtime.agent_watch.reverse.clone(),
            h.agent_runtime.agent_watch.subscriptions.clone(),
            h.agent_runtime.agent_watch.provider_status.clone(),
            h.agent_runtime.agent_watch.provider_deliveries.clone(),
        ),
        stopped_before
    );
    assert_eq!(event_log_events(&h).len(), stopped_events_before);
    h.shutdown().expect("shutdown");
}

/// Forward reachability must be iterative and visited: a long chain closes
/// without stack recursion, while a malformed unrelated loop terminates and
/// does not cause a false positive.
#[test]
fn agent_watch_reachability_is_iterative_and_cycle_defensive() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    for index in 0..4_000 {
        h.agent_runtime
            .agent_watch
            .forward
            .entry(format!("node-{index}"))
            .or_default()
            .insert(format!("node-{}", index + 1));
    }
    h.agent_runtime
        .agent_watch
        .forward
        .entry("malformed-a".to_owned())
        .or_default()
        .insert("malformed-b".to_owned());
    h.agent_runtime
        .agent_watch
        .forward
        .entry("malformed-b".to_owned())
        .or_default()
        .insert("malformed-a".to_owned());

    assert!(h.agent_watch_path_exists("node-0", "node-4000"));
    assert!(!h.agent_watch_path_exists("malformed-a", "node-4000"));
    h.shutdown().expect("shutdown");
}

/// The configured retry boundary must hide attempt N without consuming its
/// category, deliver that category at N+1, retain later category dedupe, and
/// deliver a terminal failure even when its attempt is within the threshold.
#[test]
fn agent_watch_provider_retry_threshold_enforces_exact_delivery_boundary() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let late_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    let late_id = durable_agent_id_for_conversation(&h, &late_cid).to_string();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watcher_cid)
        .expect("watcher")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("watcher-busy"),
    };
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    let session_id = h.session_runtime.current_session_id.clone();
    let retry_status = |category, attempt| tau_proto::AgentWatchProviderStatusNotification {
        session_id: session_id.clone(),
        subscription_id: String::new(),
        turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(1),
        agent_prompt_id: test_agent_prompt_id("sp-threshold-boundary"),
        state: tau_proto::AgentWatchProviderState::Retrying {
            category,
            attempt,
            next_retry_delay_secs: 1,
        },
        initial: false,
    };
    for (category, attempt) in [
        (tau_proto::AgentWatchProviderCategory::Transport, 5),
        (tau_proto::AgentWatchProviderCategory::Throttle, 5),
    ] {
        h.update_agent_watch_provider_status(&watched_id, retry_status(category, attempt));
    }
    h.set_agent_watch(
        &late_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let late_snapshot = session_agent_message_received_events(&h)
        .into_iter()
        .rev()
        .find(|message| message.recipient_id.as_str() == late_id)
        .and_then(|message| message.watch_provider_status)
        .expect("suppressed retry remains available as an initial snapshot");
    assert!(late_snapshot.initial);
    assert!(matches!(
        late_snapshot.state,
        tau_proto::AgentWatchProviderState::Retrying {
            category: tau_proto::AgentWatchProviderCategory::Throttle,
            attempt: 5,
            ..
        }
    ));
    assert!(
        h.agent_runtime.agent_registry.agents[&late_cid]
            .dispatch
            .pending_prompts
            .is_empty(),
        "a suppressed late-watch snapshot must not prompt the watcher"
    );
    for (category, attempt) in [
        (tau_proto::AgentWatchProviderCategory::Transport, 6),
        (tau_proto::AgentWatchProviderCategory::Transport, 7),
        (tau_proto::AgentWatchProviderCategory::Throttle, 7),
    ] {
        h.update_agent_watch_provider_status(&watched_id, retry_status(category, attempt));
    }
    h.update_agent_watch_provider_status(
        &watched_id,
        tau_proto::AgentWatchProviderStatusNotification {
            session_id,
            subscription_id: String::new(),
            turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(1),
            agent_prompt_id: test_agent_prompt_id("sp-threshold-terminal"),
            state: tau_proto::AgentWatchProviderState::TerminalError {
                failure_kind: tau_proto::ProviderFailureKind::RequestRejected,
                attempt: 5,
            },
            initial: false,
        },
    );

    let states: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter_map(|message| {
            (message.recipient_id.as_str() == watcher_id)
                .then_some(message.watch_provider_status?.state)
        })
        .collect();
    assert_eq!(
        states,
        [
            tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Transport,
                attempt: 6,
                next_retry_delay_secs: 1,
            },
            tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Throttle,
                attempt: 7,
                next_retry_delay_secs: 1,
            },
            tau_proto::AgentWatchProviderState::TerminalError {
                failure_kind: tau_proto::ProviderFailureKind::RequestRejected,
                attempt: 5,
            },
        ]
    );
    h.shutdown().expect("shutdown");
}

/// A zero retry threshold must preserve category-deduplicated delivery from the
/// first attempt rather than turning duplicate same-category updates into
/// prompts.
#[test]
fn agent_watch_provider_retry_threshold_zero_preserves_category_dedupe() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config
        .accepted_harness_settings
        .agent_watch_retry_notification_threshold = AgentWatchRetryNotificationPolicy::from_raw(0);
    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watcher_cid)
        .expect("watcher")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("watcher-busy"),
    };
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    let session_id = h.session_runtime.current_session_id.clone();
    let status = |category, attempt| tau_proto::AgentWatchProviderStatusNotification {
        session_id: session_id.clone(),
        subscription_id: String::new(),
        turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(1),
        agent_prompt_id: test_agent_prompt_id("sp-zero-threshold"),
        state: tau_proto::AgentWatchProviderState::Retrying {
            category,
            attempt,
            next_retry_delay_secs: 1,
        },
        initial: false,
    };
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderCategory::Transport, 0),
    );
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderCategory::Transport, 1),
    );
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderCategory::Throttle, 1),
    );

    let retries: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter_map(|message| {
            (message.recipient_id.as_str() == watcher_id)
                .then_some(message.watch_provider_status?.state)
        })
        .collect();
    assert_eq!(
        retries,
        [
            tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Transport,
                attempt: 0,
                next_retry_delay_secs: 1,
            },
            tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Throttle,
                attempt: 1,
                next_retry_delay_secs: 1,
            },
        ]
    );
    h.shutdown().expect("shutdown");
}

/// Unloading a watcher during provider retry must retire all of its incoming
/// and outgoing subscriptions before later recovery or terminal fanout can
/// append durable recipient facts.
#[test]
fn unloading_agent_watcher_retires_topology_and_stops_durable_fanout() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let upstream_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    let upstream_id = durable_agent_id_for_conversation(&h, &upstream_cid).to_string();
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_watch(
        &upstream_id,
        &watcher_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let session_id = h.session_runtime.current_session_id.clone();
    let status = |state| tau_proto::AgentWatchProviderStatusNotification {
        session_id: session_id.clone(),
        subscription_id: String::new(),
        turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(1),
        agent_prompt_id: test_agent_prompt_id("watcher-unload-prompt"),
        state,
        initial: false,
    };
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderState::Retrying {
            category: tau_proto::AgentWatchProviderCategory::Transport,
            attempt: 1,
            next_retry_delay_secs: 1,
        }),
    );
    h.update_agent_watch_provider_status(
        &watcher_id,
        tau_proto::AgentWatchProviderStatusNotification {
            session_id: session_id.clone(),
            subscription_id: String::new(),
            turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(1),
            agent_prompt_id: test_agent_prompt_id("unloaded-watcher-status"),
            state: tau_proto::AgentWatchProviderState::RecoveringContext { attempt: 1 },
            initial: false,
        },
    );

    h.remove_agent(&watcher_cid);
    assert!(
        !h.agent_runtime
            .agent_watch
            .forward
            .contains_key(&watcher_id)
    );
    assert!(
        !h.agent_runtime
            .agent_watch
            .reverse
            .contains_key(&watcher_id)
    );
    // Exercise the local fallback after the committed unload reaction.
    h.retire_agent_watch_endpoint(
        &watcher_id,
        Some(tau_proto::AgentWatchLifecycleReason::UnexpectedUnload),
    );
    let durable_before = h
        .session_runtime
        .agent_store
        .agent_events(&watcher_id)
        .expect("watcher durable log")
        .len();
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderState::RecoveringContext { attempt: 2 }),
    );
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderState::TerminalError {
            failure_kind: tau_proto::ProviderFailureKind::ContextWindowExceeded,
            attempt: 2,
        }),
    );

    assert_eq!(
        h.session_runtime
            .agent_store
            .agent_events(&watcher_id)
            .expect("watcher durable log")
            .len(),
        durable_before,
        "post-unload provider phases must not append recipient facts"
    );
    assert!(
        !h.agent_runtime
            .agent_watch
            .forward
            .contains_key(&watcher_id)
    );
    assert!(
        !h.agent_runtime
            .agent_watch
            .reverse
            .contains_key(&watcher_id)
    );
    assert!(
        h.agent_runtime
            .agent_watch
            .subscriptions
            .keys()
            .all(|(watcher, watched)| watcher != &watcher_id && watched != &watcher_id)
    );
    assert!(
        !h.agent_runtime
            .agent_watch
            .provider_status
            .contains_key(&watcher_id)
    );
    assert!(
        h.agent_runtime
            .agent_watch
            .provider_deliveries
            .keys()
            .all(|subscription| h
                .agent_runtime
                .agent_watch
                .subscriptions
                .values()
                .any(|id| id == subscription))
    );
    h.shutdown().expect("shutdown");
}

/// Per-agent turn generation cleanup must not erase another watched agent's
/// active same-category dedupe bucket.
#[test]
fn agent_watch_provider_dedupe_isolated_across_watched_agents() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config
        .accepted_harness_settings
        .agent_watch_retry_notification_threshold = AgentWatchRetryNotificationPolicy::from_raw(0);
    let a_cid = ensure_test_user_agent(&mut h);
    let b_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let a_id = durable_agent_id_for_conversation(&h, &a_cid).to_string();
    let b_id = durable_agent_id_for_conversation(&h, &b_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watcher_cid)
        .expect("watcher")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("watcher-busy"),
    };
    for watched in [&a_id, &b_id] {
        h.set_agent_watch(
            &watcher_id,
            watched,
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        );
    }
    let session_id = h.session_runtime.current_session_id.clone();
    let b_status = |attempt| tau_proto::AgentWatchProviderStatusNotification {
        session_id: session_id.clone(),
        subscription_id: String::new(),
        turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(1),
        agent_prompt_id: test_agent_prompt_id("sp-b-retry"),
        state: tau_proto::AgentWatchProviderState::Retrying {
            category: tau_proto::AgentWatchProviderCategory::Transport,
            attempt,
            next_retry_delay_secs: attempt,
        },
        initial: false,
    };
    h.update_agent_watch_provider_status(&b_id, b_status(1));
    for generation in 0..5 {
        h.set_agent_turn_state(
            &a_cid,
            AgentTurnState::AgentThinking {
                agent_prompt_id: test_agent_prompt_id(format!("sp-a-{generation}")),
            },
        );
        h.set_agent_turn_state(&a_cid, AgentTurnState::Idle);
    }
    h.update_agent_watch_provider_status(&b_id, b_status(2));
    assert_eq!(
        session_agent_message_received_events(&h)
            .iter()
            .filter(|message| {
                message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
                    && message.sender_id.as_str() == b_id
                    && !message
                        .watch_provider_status
                        .as_ref()
                        .is_some_and(|status| status.initial)
            })
            .count(),
        1
    );
    h.shutdown().expect("shutdown");
}

/// Harness mapping from provider-owned retry vocabulary to watcher vocabulary
/// must remain exhaustive and semantic.
#[test]
fn provider_retry_categories_map_to_watcher_categories() {
    for (provider, watched) in [
        (
            tau_proto::ProviderRetryCategory::Transport,
            tau_proto::AgentWatchProviderCategory::Transport,
        ),
        (
            tau_proto::ProviderRetryCategory::Overload,
            tau_proto::AgentWatchProviderCategory::Overload,
        ),
        (
            tau_proto::ProviderRetryCategory::Throttle,
            tau_proto::AgentWatchProviderCategory::Throttle,
        ),
        (
            tau_proto::ProviderRetryCategory::UsageWindow,
            tau_proto::AgentWatchProviderCategory::UsageWindow,
        ),
        (
            tau_proto::ProviderRetryCategory::Account,
            tau_proto::AgentWatchProviderCategory::Account,
        ),
        (
            tau_proto::ProviderRetryCategory::Auth,
            tau_proto::AgentWatchProviderCategory::Auth,
        ),
        (
            tau_proto::ProviderRetryCategory::Unknown,
            tau_proto::AgentWatchProviderCategory::Unknown,
        ),
    ] {
        assert_eq!(crate::harness::watch_category_for_retry(provider), watched);
    }
}

/// The production finished-response path retains the matching retry attempt in
/// its terminal update, clears retry state after success, and ignores duplicate
/// or stale terminal responses without exposing raw provider text.
#[test]
fn agent_watch_provider_terminal_ordering_attempt_and_success_cleanup() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watcher_cid)
        .expect("watcher")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("watcher-busy"),
    };
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    let terminal_prompt: tau_proto::AgentPromptId = test_agent_prompt_id("sp-watch-terminal");
    seed_agent_thinking(&mut h, &watched_cid, terminal_prompt.as_str());
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watched_cid)
        .expect("watched")
        .turn
        .published_runtime_state = tau_proto::AgentRuntimeState::Running;
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watched_cid)
        .expect("watched")
        .turn
        .turn_generation = tau_proto::AgentOuterTurnGeneration::from_raw(1);
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(terminal_prompt.clone(), watched_cid.clone());
    let generation = h.agent_runtime.agent_registry.agents[&watched_cid]
        .turn
        .turn_generation;
    h.update_agent_watch_provider_status(
        &watched_id,
        tau_proto::AgentWatchProviderStatusNotification {
            session_id: h.session_runtime.current_session_id.clone(),
            subscription_id: String::new(),
            turn_generation: generation,
            agent_prompt_id: terminal_prompt.clone(),
            state: tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Throttle,
                attempt: 17,
                next_retry_delay_secs: 30,
            },
            initial: false,
        },
    );
    let terminal = ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: terminal_prompt.clone(),
        agent_id: crate::parse_agent_id(&watched_id),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("secret raw endpoint response".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::RequestRejected),
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    };
    h.handle_provider_response_finished(terminal.clone())
        .expect("terminal response");

    let watched_edges: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| {
            message.recipient_id.as_str() == watcher_id
                && message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
        })
        .collect();
    let terminal_index = watched_edges
        .iter()
        .position(|message| {
            matches!(
                message
                    .watch_provider_status
                    .as_ref()
                    .map(|status| &status.state),
                Some(tau_proto::AgentWatchProviderState::TerminalError {
                    failure_kind: tau_proto::ProviderFailureKind::RequestRejected,
                    attempt: 18,
                })
            )
        })
        .expect("terminal status");
    let retry_index = watched_edges
        .iter()
        .position(|message| {
            matches!(
                message
                    .watch_provider_status
                    .as_ref()
                    .map(|status| &status.state),
                Some(tau_proto::AgentWatchProviderState::Retrying { attempt: 17, .. })
            )
        })
        .expect("retry status");
    assert!(retry_index < terminal_index);
    assert!(
        watched_edges
            .iter()
            .all(|message| !message.message.contains("secret raw endpoint response"))
    );
    let before_duplicate = watched_edges.len();
    h.handle_provider_response_finished(terminal)
        .expect("duplicate terminal is ignored");
    assert_eq!(
        session_agent_message_received_events(&h)
            .into_iter()
            .filter(|message| {
                message.recipient_id.as_str() == watcher_id
                    && message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
            })
            .count(),
        before_duplicate
    );

    let success_prompt: tau_proto::AgentPromptId = test_agent_prompt_id("sp-watch-success");
    seed_agent_thinking(&mut h, &watched_cid, success_prompt.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(success_prompt.clone(), watched_cid.clone());
    h.update_agent_watch_provider_status(
        &watched_id,
        tau_proto::AgentWatchProviderStatusNotification {
            session_id: h.session_runtime.current_session_id.clone(),
            subscription_id: String::new(),
            turn_generation: h.agent_runtime.agent_registry.agents[&watched_cid]
                .turn
                .turn_generation,
            agent_prompt_id: success_prompt.clone(),
            state: tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Transport,
                attempt: 3,
                next_retry_delay_secs: 4,
            },
            initial: false,
        },
    );
    h.handle_provider_response_finished(provider_text_response(
        &success_prompt,
        crate::parse_agent_id(&watched_id),
        "completed",
    ))
    .expect("successful response");
    assert!(
        !h.agent_runtime
            .agent_watch
            .provider_status
            .contains_key(&watched_id),
        "success must clear a prior retry snapshot"
    );

    let first_attempt_prompt: tau_proto::AgentPromptId =
        test_agent_prompt_id("sp-watch-first-terminal");
    seed_agent_thinking(&mut h, &watched_cid, first_attempt_prompt.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(first_attempt_prompt.clone(), watched_cid.clone());
    let mut first_attempt_terminal = provider_text_response(
        &first_attempt_prompt,
        crate::parse_agent_id(&watched_id),
        "",
    );
    first_attempt_terminal.stop_reason = tau_proto::ProviderStopReason::Error;
    first_attempt_terminal.error = Some("raw first-attempt error".to_owned());
    first_attempt_terminal.failure_kind = Some(tau_proto::ProviderFailureKind::RequestRejected);
    h.handle_provider_response_finished(first_attempt_terminal)
        .expect("first-attempt terminal");
    assert!(matches!(
        h.agent_runtime.agent_watch.provider_status[&watched_id].state,
        tau_proto::AgentWatchProviderState::TerminalError { attempt: 1, .. }
    ));
    h.shutdown().expect("shutdown");
}

#[test]
fn disabling_agent_watch_removes_response_fanout_route() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let sender_cid = ensure_test_user_agent(&mut h);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&sender_cid)
        .expect("sender")
        .identity
        .agent_id = Some(crate::parse_agent_id("child-agent"));
    h.agent_runtime
        .agent_registry
        .agent_routes
        .insert(crate::parse_agent_id("child-agent"), sender_cid.clone());
    h.set_agent_watch(
        "watcher-agent",
        "child-agent",
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_watch(
        "watcher-agent",
        "child-agent",
        false,
        tau_proto::AgentWatchUpdateCause::AgentWatchDisable,
    );
    assert!(
        h.watchers_for_agent("child-agent").is_empty(),
        "disabled watch must remove the child from watch-response fan-out"
    );
    h.shutdown().expect("shutdown");
}

/// Cross-harness message RPCs must validate the active target session and then
/// publish only the harness-owned recipient projection with external sender
/// identity preserved for prompt/UI rendering.
#[test]
fn external_agent_message_request_publishes_received_projection() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("conversation")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("external-message-target"),
    };

    let result = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "external-ok".to_owned(),
            message_id: tau_proto::AgentMessageId::parse("msg-external-ok")
                .expect("test identifier must satisfy its grammar"),
            capability: "cap-ok".to_owned(),
            sender_session_id: test_session_id("other-session"),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: test_session_id("s1"),
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
                &recipient_id,
            )),
            kind: tau_proto::AgentMessageKind::Message,
            message: "hello from outside".to_owned(),
        },
    );

    assert_eq!(result.request_id, "external-ok");
    assert_eq!(result.failure, None);
    assert!(session_agent_message_sent_events(&h).is_empty());
    let received = session_agent_message_received_events(&h);
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].sender_id.as_str(), "sender_agent");
    assert_eq!(
        received[0].sender_session_id.as_deref(),
        Some("other-session")
    );
    assert_eq!(received[0].recipient_id.as_str(), recipient_id.as_str());
    assert_eq!(received[0].message, "hello from outside");
    let recipient = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("recipient conversation");
    assert!(recipient.dispatch.pending_prompts.is_empty());
    assert!(matches!(
        recipient.dispatch.pending_message_wakes.front(),
        Some(crate::agent::PendingMessageWake {
            source: crate::agent::PendingMessageWakeSource::AgentMessageReceived { .. },
            ..
        })
    ));
    let context = crate::prompt::assemble_prompt_context_from(
        h.session_runtime
            .agent_store
            .agent(&recipient_id)
            .expect("recipient tree"),
        recipient.identity.head,
    )
    .context
    .flatten();
    assert!(context.iter().any(|item| {
        text_part(item).is_some_and(|text| {
            text.contains(
                "<tau_peer_message sender_session=\"other-session\" sender_agent=\"sender_agent\">",
            ) && text.contains("hello from outside")
        })
    }));

    h.shutdown().expect("shutdown");
}

/// Cross-harness message RPCs are addressed to one immutable session; a
/// wrong-session request must not deliver into that session.
#[test]
fn external_agent_message_request_rejects_wrong_active_session() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");

    let result = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "external-wrong-session".to_owned(),
            message_id: tau_proto::AgentMessageId::parse("msg-external-wrong-session")
                .expect("test identifier must satisfy its grammar"),
            capability: "cap-wrong-session".to_owned(),
            sender_session_id: test_session_id("other-session"),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: test_session_id("not-s1"),
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
                &recipient_id,
            )),
            kind: tau_proto::AgentMessageKind::Message,
            message: "hello from outside".to_owned(),
        },
    );

    assert_eq!(result.request_id, "external-wrong-session");
    assert_eq!(
        result.failure,
        Some(tau_proto::ExternalAgentMessageFailure::TargetSessionChanged)
    );
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Cross-harness message RPCs must reject unknown recipients before writing a
/// durable inbound transcript projection.
#[test]
fn external_agent_message_request_rejects_unknown_recipient() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let result = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "external-unknown".to_owned(),
            message_id: tau_proto::AgentMessageId::parse("msg-external-unknown")
                .expect("test identifier must satisfy its grammar"),
            capability: "cap-unknown".to_owned(),
            sender_session_id: test_session_id("other-session"),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: test_session_id("s1"),
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
                "missing_agent",
            )),
            kind: tau_proto::AgentMessageKind::Message,
            message: "hello from outside".to_owned(),
        },
    );

    assert_eq!(result.request_id, "external-unknown");
    assert_eq!(
        result.failure,
        Some(tau_proto::ExternalAgentMessageFailure::RecipientUnknown)
    );
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Cross-harness message RPCs must reject empty messages before writing a
/// durable inbound transcript projection.
#[test]
fn external_agent_message_request_rejects_empty_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");

    let result = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "external-empty".to_owned(),
            message_id: tau_proto::AgentMessageId::parse("msg-external-empty")
                .expect("test identifier must satisfy its grammar"),
            capability: "cap-empty".to_owned(),
            sender_session_id: test_session_id("other-session"),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: test_session_id("s1"),
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
                &recipient_id,
            )),
            kind: tau_proto::AgentMessageKind::Message,
            message: " \n\t ".to_owned(),
        },
    );

    assert_eq!(result.request_id, "external-empty");
    assert_eq!(
        result.failure,
        Some(tau_proto::ExternalAgentMessageFailure::Rejected)
    );
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Production external-message intake must reject invalid target-side fields
/// before starting sender authentication. Returning `Some(result)` from
/// `start_external_agent_message_auth` is the immediate-rejection path; `None`
/// means a background sender-auth helper was started.
#[test]
fn external_agent_message_auth_start_rejects_invalid_target_before_callback() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    let client_id: tau_proto::ConnectionId = crate::test_connection_id("external");

    let base = tau_proto::ExternalAgentMessageRequest {
        request_id: "external-preauth".to_owned(),
        message_id: tau_proto::AgentMessageId::parse("msg-external-preauth")
            .expect("test identifier must satisfy its grammar"),
        capability: "cap-preauth".to_owned(),
        sender_session_id: test_session_id("other-session"),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: h.session_runtime.current_session_id.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
            &recipient_id,
        )),
        kind: tau_proto::AgentMessageKind::Message,
        message: "hello".to_owned(),
    };

    let cases = [
        (
            tau_proto::ExternalAgentMessageRequest {
                request_id: "external-preauth-wrong-session".to_owned(),
                recipient_session_id: test_session_id("wrong-session"),
                ..base.clone()
            },
            tau_proto::ExternalAgentMessageFailure::TargetSessionChanged,
        ),
        (
            tau_proto::ExternalAgentMessageRequest {
                request_id: "external-preauth-empty".to_owned(),
                message: " \n\t ".to_owned(),
                ..base.clone()
            },
            tau_proto::ExternalAgentMessageFailure::Rejected,
        ),
    ];

    for (request, expected_failure) in cases {
        let request_id = request.request_id.clone();
        let result = h
            .start_external_agent_message_auth(client_id.clone(), request)
            .expect("invalid target request should be rejected immediately");
        assert_eq!(result.request_id, request_id);
        assert_eq!(result.failure, Some(expected_failure));
    }
    let unknown = tau_proto::ExternalAgentMessageRequest {
        request_id: "external-preauth-unknown".to_owned(),
        recipient: tau_proto::ExternalAgentMessageRecipient::Exact(crate::parse_agent_id(
            "missing_agent",
        )),
        ..base
    };
    assert!(
        h.start_external_agent_message_auth(client_id, unknown)
            .is_none(),
        "exact inventory must not be consulted before sender authentication"
    );
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}
/// A selected sibling message wake may dispatch while a readiness-deferred
/// activation stays dormant on another branch; neither ownership token consumes
/// the other.
#[test]
fn readiness_deferred_activation_does_not_absorb_sibling_message_wake() {
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
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("branch-b-readiness-hold"),
    };
    h.publish_for_agent(
        &cid,
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("branch-b-message")
                .expect("test identifier must satisfy its grammar"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: agent_id.clone(),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "branch B wake".to_owned(),
        }),
    );
    let branch_b = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("branch B wake node");
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_message_wakes
            .len(),
        1,
        "branch_a={branch_a:?} branch_b={branch_b:?} head={:?} state={:?} checkpoints={:?}",
        h.agent_runtime.agent_registry.agents[&cid].identity.head,
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
            .collect::<Vec<_>>()
    );
    finish_test_agent_context_wait(&mut h, &agent_id);
    h.set_agent_turn_state(&cid, AgentTurnState::Idle);
    h.drain_publish_idle_dispatches();
    h.try_advance_queue();
    let branch_b_prompt = read_nth_prompt_created(&h, 0);
    let first_checkpoint = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentInferenceDispatchStarted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .expect("selected message checkpoint");
    assert_eq!(
        first_checkpoint.through,
        tau_proto::AgentHead::Node(branch_b)
    );
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_message_wakes
            .is_empty()
    );

    h.handle_provider_response_finished(provider_text_response(
        &branch_b_prompt.agent_prompt_id,
        agent_id.clone(),
        "branch B complete",
    ))
    .expect("finish selected message turn");
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: tau_proto::AgentHead::Node(branch_a),
        }),
    );
    let checkpoints = event_log_events(&h)
        .into_iter()
        .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
        .count();
    assert_eq!(checkpoints, 2);
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
}

/// Agent-to-agent input interrupts an exact wait only at its bounded wait-tool
/// deadline, preserving an aggregation window without stalling indefinitely.
#[test]
fn agent_message_interrupts_recipient_active_wait() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config
        .accepted_harness_settings
        .notification_delivery
        .agent_message = HarnessSettings::built_in()
        .notification_delivery
        .agent_message;
    h.config.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-msg-wait");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-msg-wait"),
        instant_background_test_tool_spec("slow_msg_wait"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("recipient id");
    let background_call_id: ToolCallId = "bg-msg-wait".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        background_call_id.as_str(),
        "slow_msg_wait",
    );

    let wait_call_id: ToolCallId = "wait-msg-interrupt".into();
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
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("start wait");
    seed_tools_running(&mut h, &cid, vec![wait_call_id.clone()]);

    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("test-message-interrupts-wait")
                .expect("test identifier must satisfy its grammar"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "please stop waiting".to_owned(),
        }),
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result) if result.call_id.as_str() == wait_call_id.as_str()
    )));
    h.process_notification_delivery_deadlines_at(Instant::now() + Duration::from_millis(120_000));

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("wait_outcome: interrupted"))
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.text.contains("please stop waiting")
    )));
    let prompt = event_log_events(&h)
        .into_iter()
        .rev()
        .find_map(|event| match event {
            Event::AgentPromptCreated(prompt)
                if prompt.agent_id.as_str() == recipient_id.as_str() =>
            {
                Some(prompt)
            }
            _ => None,
        })
        .expect("message wake prompt");
    assert!(prompt.context.flatten().iter().any(|item| {
        text_part(item).is_some_and(|text| {
            text.contains("You have received a message from manager")
                && text.contains("please stop waiting")
        })
    }));

    h.shutdown().expect("shutdown");
}

/// Regression companion to the user-prompt queued-before-wait race: inbound
/// `agent.message_received` prompts are hidden/internal in transcript terms,
/// but active waits already treat them as input that must interrupt waiting.
/// The same preemption must apply when the message is queued before `wait`
/// starts.
#[test]
fn wait_start_is_interrupted_by_already_queued_agent_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    h.config
        .accepted_harness_settings
        .notification_delivery
        .agent_message = NotificationDeliveryPolicy::from_millis(0, 1, 1)
        .expect("one-millisecond prospective wait policy");

    let _tool_events = connect_ready_configured_extension(
        &mut h,
        "conn-queued-message-wait",
        "configured-conn-queued-message-wait",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-queued-message-wait"),
        instant_background_test_tool_spec("slow_queued_message_wait"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let background_call_id: ToolCallId = "bg-queued-message-wait".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        background_call_id.as_str(),
        "slow_queued_message_wait",
    );
    let wait_call_id: ToolCallId = "wait-queued-message".into();
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
    seed_tools_running(&mut h, &cid, vec![wait_call_id.clone()]);
    let recipient_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("queued-manager-message")
                .expect("test identifier must satisfy its grammar"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "queued manager message".to_owned(),
        }),
    );
    std::thread::sleep(Duration::from_millis(2));
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("wait interrupted by queued agent message");

    assert_eq!(
        tool_result_count(&h, wait_call_id.as_str()),
        1,
        "wait should complete exactly once when queued agent input preempts it"
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("wait_outcome: interrupted"))
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered) if steered.text.contains("queued manager message")
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.context.flatten().iter().any(|item| {
                text_part(item).is_some_and(|text| text.contains("queued manager message"))
            })
    )));

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-queued-message-wait"),
        Event::ToolResultReported(final_tool_result(
            background_call_id.as_str(),
            "slow_queued_message_wait",
            "background done after message interrupt",
        )),
    )
    .expect("background result after interrupted wait");
    assert_eq!(
        tool_result_count(&h, wait_call_id.as_str()),
        1,
        "the later background result must not resume a wait that never started"
    );

    h.shutdown().expect("shutdown");
}

/// Queue-before-register correlation skips an earlier ready sibling wake and
/// settles against the first ready activation on the selected branch.
#[test]
fn wait_start_cites_selected_branch_activation_after_off_branch_wake() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let _tool_events = connect_test_tool(&mut h, "branch-wait-correlation-tool");
    h.tool_routing.registry.register(
        &crate::test_connection_id("branch-wait-correlation-tool"),
        instant_background_test_tool_spec("slow_branch_wait"),
    );
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = durable_agent_id_for_conversation(&h, &cid);
    let background_call: ToolCallId = "branch-wait-background".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        background_call.as_str(),
        "slow_branch_wait",
    );
    h.set_agent_turn_state(
        &cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: test_agent_prompt_id("branch-wait-busy"),
        },
    );
    let message = |id: &str, body: &str| {
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse(id).expect("message id"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: body.to_owned(),
        })
    };
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        message("off-branch-wait-wake", "off branch"),
    );
    let off_branch = h.agent_runtime.agent_registry.agents[&cid].identity.head;
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: recipient_id.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        message("selected-branch-wait-wake", "selected branch"),
    );
    let selected = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("selected message node");
    assert_ne!(off_branch, Some(selected));
    let wakes = &h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .pending_message_wakes;
    let selected_activation = wakes
        .iter()
        .find(|wake| wake.node_id == Some(selected))
        .and_then(|wake| wake.activation_observation)
        .expect("selected activation");

    let wait_call = AgentToolCall {
        call_ref: Some(tau_proto::ToolCallRef {
            declaration: tau_proto::ObservationId::from_bytes([91; 16]),
            item_index: 0,
        }),
        id: "branch-aware-wait".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(background_call.to_string()),
        )]),
    };
    assert_eq!(
        h.first_wait_preempting_message_activation_for_kind(&cid, DeliveryDeadlineKind::WaitTool,),
        Some(selected_activation),
        "the earlier dormant sibling cannot own wait settlement correlation"
    );
    seed_tools_running(&mut h, &cid, vec![wait_call.id.clone()]);
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("settle prospective exact wait");
    h.drain_publish_idle_dispatches();
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id == wait_call.id
                && matches!(&result.result, CborValue::Text(text) if text.contains("wait_outcome: interrupted"))
    )));
}

/// A no-tool response is not request-terminal while an accepted agent message
/// still awaits its continuation turn. Cold restore must retain the historical
/// extension ownership rather than classify this interrupted request as a
/// completed worker.
#[test]
fn cold_restore_does_not_detach_worker_with_message_continuation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let (worker_agent_id, parent_agent_id) = {
        let mut h = echo_harness(&sp).expect("start");
        h.config.selected_model = Some("test/model".into());
        let parent_cid = ensure_test_user_agent(&mut h);
        let parent_agent_id = durable_agent_id_for_conversation(&h, &parent_cid);
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert("message-cut-call".into(), parent_cid);
        let mut query = ext_query("delegate-3");
        query.tool_call_id = Some("message-cut-call".into());
        h.handle_start_agent_request(&crate::test_connection_id(HARNESS_CONNECTION_ID), query)
            .expect("start worker");
        let worker_cid = ext_query_cid(&h, "delegate-3").expect("worker");
        let worker_agent_id = durable_agent_id_for_conversation(&h, &worker_cid);
        let first_prompt_id = h
            .prompt_coordination
            .prompt_runtime
            .agents
            .iter()
            .find_map(|(prompt_id, cid)| (cid == &worker_cid).then_some(prompt_id.clone()))
            .expect("first worker prompt");
        h.publish_event(
            Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
            Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
                message_id: tau_proto::AgentMessageId::parse("message-cut-delivery")
                    .expect("test identifier must satisfy its grammar"),
                sender_id: crate::parse_agent_id("sender"),
                sender_session_id: None,
                recipient_id: worker_agent_id.clone(),
                kind: tau_proto::AgentMessageKind::Message,
                watch_provider_status: None,
                watch_work_status: None,
                watch_long_wait: None,
                watch_lifecycle: None,
                message: "continue before completion".to_owned(),
            }),
        );
        let mut response =
            provider_text_response(&first_prompt_id, worker_agent_id.clone(), "first answer");
        response.originator = tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name(HARNESS_CONNECTION_ID),
            query_id: "delegate-3".to_owned(),
        };
        h.handle_provider_response_finished(response)
            .expect("finish first worker round");
        assert!(matches!(
            h.agent_runtime.agent_registry.agents[&worker_cid]
                .identity
                .originator,
            tau_proto::PromptOriginator::Extension { .. }
        ));
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .agents
                .iter()
                .any(|(prompt_id, cid)| cid == &worker_cid && prompt_id != &first_prompt_id)
        );
        h.handle_authenticated_ui_prompt_submitted(
            crate::harness::harness_connection_id(),
            UiPromptSubmitted {
                literal: false,
                session_id: test_session_id("s1"),
                text: "keep coordinating".to_owned(),
                agent_id: parent_agent_id.clone(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            },
        )
        .expect("leave coordinator interrupted");
        h.shutdown().expect("shutdown continuation cut");
        (worker_agent_id, parent_agent_id)
    };

    let mut resumed =
        echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
            .expect("resume continuation cut");
    let worker_cid = resumed
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(worker_agent_id.as_str())
        .cloned()
        .expect("interrupted worker remains routed");
    assert!(matches!(
        resumed.agent_runtime.agent_registry.agents[&worker_cid]
            .identity
            .originator,
        tau_proto::PromptOriginator::Extension { .. }
    ));
    assert_eq!(
        resumed.agent_runtime.agent_registry.agents[&worker_cid]
            .identity
            .source_connection
            .as_ref(),
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID))
    );
    assert!(
        resumed.agent_runtime.agent_registry.agents[&worker_cid]
            .identity
            .restored_tool_backed_start
    );
    assert_eq!(
        path_crate_internal_tools::InternalToolHost::new(&mut resumed)
            .agent_id_for_harness_start_query("delegate-3"),
        Some(worker_agent_id.to_string())
    );
    let parent_cid = resumed
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(parent_agent_id.as_str())
        .cloned()
        .expect("restored parent");
    assert_eq!(
        resumed.agent_runtime.agent_registry.agents[&worker_cid]
            .identity
            .parent_agent_id,
        Some(parent_cid.clone())
    );
    resumed
        .try_set_agent_watch(
            parent_agent_id.as_str(),
            worker_agent_id.as_str(),
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        )
        .expect("watch restored worker");
    resumed
        .publish_agent_message_from_agent(
            &parent_cid,
            worker_agent_id.to_string(),
            "continue after restart".to_owned(),
        )
        .expect("message restored worker");
    let continued_prompt_id = resumed.agent_runtime.agent_registry.agents[&worker_cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("message continuation prompt");
    resumed.register_harness_tools();
    let mut reply = provider_text_response(&continued_prompt_id, worker_agent_id.clone(), "");
    reply.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
    reply.output_items = vec![ContextItem::ToolCall(ToolCallItem {
        call_id: "reply-parent".into(),
        name: ToolName::new("message"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("recipient_id".to_owned()),
                CborValue::Text(parent_agent_id.to_string()),
            ),
            (
                CborValue::Text("message".to_owned()),
                CborValue::Text("restart acknowledged".to_owned()),
            ),
        ]),
        raw_arguments_json: None,
        responses_envelope: None,
    })];
    reply.originator = tau_proto::PromptOriginator::Extension {
        name: crate::test_extension_name(HARNESS_CONNECTION_ID),
        query_id: "delegate-3".to_owned(),
    };
    resumed
        .handle_provider_response_finished(reply)
        .expect("worker replies to parent");
    let final_prompt_id = resumed.agent_runtime.agent_registry.agents[&worker_cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("worker continuation after reply");
    let mut final_response =
        provider_text_response(&final_prompt_id, worker_agent_id.clone(), "worker final");
    final_response.originator = tau_proto::PromptOriginator::Extension {
        name: crate::test_extension_name(HARNESS_CONNECTION_ID),
        query_id: "delegate-3".to_owned(),
    };
    resumed
        .handle_provider_response_finished(final_response)
        .expect("complete restored worker");
    assert_eq!(
        event_log_events(&resumed)
            .iter()
            .filter(|event| matches!(
                event,
                Event::StartAgentResult(result) if result.query_id == "delegate-3"
            ))
            .count(),
        1
    );
    assert_eq!(
        resumed
            .session_runtime
            .agent_store
            .agent_events(parent_agent_id.as_str())
            .expect("parent journal")
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentMessageReceived(message)
                    if message.kind == tau_proto::AgentMessageKind::WatchResponse
                        && message.sender_id == worker_agent_id
            ))
            .count(),
        1
    );
    assert!(
        resumed
            .agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(worker_agent_id.as_str())
    );
    assert!(!event_log_contains_any_source(&resumed, |event| matches!(
        event,
        Event::SessionAgentUnloaded(unloaded) if unloaded.agent_id == worker_agent_id
    )));
    resumed.shutdown().expect("shutdown resumed cut");
}

/// A live side request that loses its completion route must preserve the exact
/// structured diagnostic and notify each surviving watcher once before prune.
#[test]
fn route_loss_emits_one_typed_lifecycle_before_watch_prune() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _extension = connect_test_tool(&mut h, "route-loss-extension");
    let watcher_cid = ensure_test_user_agent(&mut h);
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid);
    let second_watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let second_watcher_id = durable_agent_id_for_conversation(&h, &second_watcher_cid);
    h.handle_start_agent_request(
        &crate::test_connection_id("route-loss-extension"),
        ext_query("route-loss-query"),
    )
    .expect("start side request");
    let worker_cid = ext_query_cid(&h, "route-loss-query").expect("worker");
    let worker_id = durable_agent_id_for_conversation(&h, &worker_cid);
    h.set_agent_watch(
        watcher_id.as_str(),
        worker_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_watch(
        second_watcher_id.as_str(),
        worker_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&worker_cid)
        .expect("worker")
        .identity
        .source_connection = None;
    let prompt_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&worker_cid)
        .and_then(|agent| agent.dispatch.in_flight_prompt.clone())
        .expect("worker prompt");
    let mut terminal = provider_text_response(&prompt_id, worker_id.clone(), "terminal");
    terminal.originator = tau_proto::PromptOriginator::Extension {
        name: crate::test_extension_name("route-loss-extension"),
        query_id: "route-loss-query".to_owned(),
    };
    h.handle_provider_response_finished(terminal)
        .expect("finish route-lost request");

    let lifecycle = session_agent_message_received_events(&h);
    for expected_watcher in [&watcher_id, &second_watcher_id] {
        let deliveries: Vec<_> = lifecycle
            .iter()
            .filter(|message| {
                message.sender_id == worker_id
                    && message.recipient_id == *expected_watcher
                    && message.kind == tau_proto::AgentMessageKind::WatchLifecycle
            })
            .collect();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].message, "");
        assert_eq!(
            deliveries[0].watch_lifecycle,
            Some(tau_proto::AgentWatchLifecycleNotification {
                state: tau_proto::AgentWatchLifecycleState::Stopped,
                reason: tau_proto::AgentWatchLifecycleReason::RestoredDelegationRouteLost,
            })
        );
    }
    assert!(h.watchers_for_agent(worker_id.as_str()).is_empty());
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.kind == tau_proto::notice_kind::HARNESS_FAILURE
                && notice.message == format!(
                    "agent_id={worker_id} query_id=route-loss-query \
                     extension=route-loss-extension reason=no_source_connection action=unload"
                )
    )));
    h.shutdown().expect("shutdown");
}

/// Expected endpoint cleanup prunes topology without falsely reporting the
/// teardown to a surviving watcher as an unexpected failure.
#[test]
fn expected_watched_agent_cleanup_prunes_without_failure_lifecycle() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid);
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid);
    h.set_agent_watch(
        watcher_id.as_str(),
        watched_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    h.remove_agent_expected(&watched_cid);

    assert!(h.watchers_for_agent(watched_id.as_str()).is_empty());
    assert!(
        session_agent_message_received_events(&h)
            .into_iter()
            .all(|message| message.kind != tau_proto::AgentMessageKind::WatchLifecycle)
    );
    h.shutdown().expect("shutdown");
}

/// A loaded-target durable message wake refreshes session retention at its
/// accepted activation boundary without changing canonical creation time.
#[test]
fn durable_message_wake_extends_session_retention() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .as_deref()
        .map(crate::parse_agent_id)
        .expect("durable agent id");
    let capture = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(TraceCaptureLayer {
        capture: capture.clone(),
    });
    let meta_path = tracing::subscriber::with_default(subscriber, || {
        h.extensions.resolving_initial_collisions = true;
        h.publish_event(
            Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
            Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
                message_id: tau_proto::AgentMessageId::parse("retention-message")
                    .expect("message id"),
                sender_id: crate::parse_agent_id("manager"),
                sender_session_id: None,
                recipient_id: agent_id,
                kind: tau_proto::AgentMessageKind::Message,
                watch_provider_status: None,
                watch_work_status: None,
                watch_long_wait: None,
                watch_lifecycle: None,
                message: "MESSAGE-WAKE-CANARY".to_owned(),
            }),
        );
        let meta_path = stale_session_manifest(&h);

        h.extensions.resolving_initial_collisions = false;
        h.try_advance_queue();
        meta_path
    });

    assert_session_manifest_refreshed(&meta_path);
    assert!(
        capture
            .events
            .lock()
            .expect("trace capture lock")
            .is_empty(),
        "message wake session activity must not enter prompt-acceptance tracing"
    );
    h.shutdown().expect("shutdown");
}

/// A strict template error hidden behind the payload-envelope provenance notice
/// must fail preflight before Tau commits an inference-dispatch checkpoint.
#[test]
fn message_fact_payload_envelope_notice_failure_precedes_dispatch_checkpoint() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .as_deref()
        .map(crate::parse_agent_id)
        .expect("durable agent id");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("user agent")
        .dispatch
        .terminating = true;
    h.commit_message_fact(
        None,
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::parse("bridge")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            tau_proto::MessageAgentTarget::new(agent_id.as_str()),
            tau_proto::MessageFactId::new("m1"),
            tau_proto::MessageParty {
                stable_id: "u1".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            None,
            "message fact",
        )),
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("user agent")
        .dispatch
        .terminating = false;
    h.prompt_coordination.context_discovery.system_prompt_templates.insert(
        "message-fact-conditional".to_owned(),
        "{{#if payload_envelope_provenance_notice}}{{missing_strict_value}}{{else}}READY{{/if}}"
            .to_owned(),
    );
    let selected_role = h.config.selected_role.clone();
    h.config
        .available_roles
        .entry(selected_role)
        .or_default()
        .prompt_override = Some("message-fact-conditional".to_owned());
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("user agent")
        .dispatch
        .pending_replay_activation = true;
    let checkpoints_before = event_log_events(&h)
        .iter()
        .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
        .count();
    let meta_path = stale_session_manifest(&h);
    crate::prompt::reset_prompt_preflight_test_counters();

    h.try_advance_queue();

    assert_eq!(
        crate::prompt::prompt_context_construction_count(),
        0,
        "production render preflight must not construct provider context"
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        checkpoints_before,
        "conditional render failure must precede the durable checkpoint"
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert!(
        h.runtime_io
            .replayable_harness_notices
            .iter()
            .any(|notice| {
                notice.purpose == tau_proto::NoticePurpose::Alert
                    && notice.message.contains("until its template is repaired")
            })
    );
    let meta: tau_core::SessionMeta =
        serde_json::from_slice(&path_std_fs::read(meta_path).expect("read session manifest"))
            .expect("decode session manifest");
    assert_eq!(meta.created_at, 7);
    assert_eq!(
        meta.last_touched, 8,
        "rejected preflight must not extend retention"
    );
    h.shutdown().expect("shutdown");
}

#[test]
fn side_agent_repetition_response_propagates_error_result() {
    // Empty repetition/error provider responses from extension-originated side
    // agents must not look like successful empty delegation results.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let frames = connect_test_client(&mut h, "conn-side", tau_proto::ClientKind::External);

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-side"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-repetition".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");
    let (side_spid, side_cid) = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default").then(|| (spid.clone(), prompt_cid.clone()))
        })
        .expect("side prompt id");
    let side_agent_id = durable_agent_id_for_conversation(&h, &side_cid);
    let mut response = provider_repetition_response(
        &side_spid,
        tau_proto::AgentId::parse("side").expect("agent id"),
    );
    response.originator = tau_proto::PromptOriginator::Extension {
        name: crate::test_extension_name("conn-side"),
        query_id: "q-repetition".to_owned(),
    };
    response.error = Some("provider stream repetition detected".to_owned());

    h.handle_provider_response_finished(response)
        .expect("side response handled");

    assert!(
        !h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&side_cid)
    );
    assert!(
        !h.agent_runtime
            .agent_registry
            .session_loaded
            .contains(&side_agent_id)
    );
    assert!(
        h.session_runtime
            .store
            .session_events("s1")
            .expect("session events")
            .iter()
            .any(|record| matches!(
                &record.event,
                Event::SessionAgentUnloaded(unloaded) if unloaded.agent_id == side_agent_id
            ))
    );
    let result = frames
        .lock()
        .expect("frames")
        .iter()
        .find_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == "q-repetition" => {
                Some(result.clone())
            }
            _ => None,
        })
        .expect("start-agent result routed");
    assert!(result.text.is_empty());
    assert_eq!(result.error.as_deref(), Some("provider failure: unknown"));
}

#[test]
fn side_agent_error_response_propagates_error_result() {
    // Plain provider errors with no assistant text should also reach the
    // StartAgentResult error field instead of becoming silent empty text.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let frames = connect_test_client(&mut h, "conn-side-error", tau_proto::ClientKind::External);

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-side-error"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-error".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");
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

        agent_prompt_id: side_spid.clone(),
        agent_id: tau_proto::AgentId::parse("side").expect("agent id"),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("provider failed".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("conn-side-error"),
            query_id: "q-error".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response handled");

    let result = frames
        .lock()
        .expect("frames")
        .iter()
        .find_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == "q-error" => {
                Some(result.clone())
            }
            _ => None,
        })
        .expect("start-agent result routed");
    assert!(result.text.is_empty());
    assert_eq!(
        result.error.as_deref(),
        Some("provider failure: context_window_exceeded")
    );
    assert!(
        !format!("{result:?}").contains("provider failed"),
        "raw provider diagnostics must not cross the delegated result boundary"
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .agents
            .contains_key(&side_spid)
    );
    assert!(h.session_runtime.turn_state.is_idle());
}

/// A delegated output-limit terminal must return an explicit error rather than
/// completing or detaching the worker as a successful result, both for
/// reasoning-only output and for partial assistant prose (the original bug
/// created an error only for empty text).
#[test]
fn side_agent_output_length_never_completes_successfully() {
    let cases = [
        (
            "reasoning only",
            vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "incomplete delegated reasoning".to_owned(),
            })],
        ),
        (
            "partial prose",
            vec![ContextItem::Message(MessageItem {
                role: tau_proto::ContextRole::Assistant,
                content: vec![tau_proto::ContentPart::Text {
                    text: "partial delegated answer".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
        ),
    ];
    for (index, (name, output_items)) in cases.into_iter().enumerate() {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path()).expect("start");
        h.config.selected_model = Some("test/model".into());
        let frames =
            connect_test_client(&mut h, "conn-side-length", tau_proto::ClientKind::External);
        h.handle_start_agent_request(
            &crate::test_connection_id("conn-side-length"),
            StartAgentRequest {
                trusted_internal_spans: Vec::new(),
                parent_agent: None,
                query_id: "q-length".to_owned(),
                instruction: "Finish within the limit.".to_owned(),
                role: None,
                input_stats: tau_proto::ToolUseStats::default(),
                tool_call_id: None,
                task_name: None,
            },
        )
        .expect("start side agent");
        let side_spid = h
            .prompt_coordination
            .prompt_runtime
            .agents
            .iter()
            .find_map(|(spid, prompt_cid)| {
                (prompt_cid.as_str() != "default").then_some(spid.clone())
            })
            .expect("side prompt id");
        h.handle_provider_response_finished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            agent_prompt_id: side_spid,
            agent_id: tau_proto::AgentId::parse("side").expect("agent id"),
            output_items,
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            originator: tau_proto::PromptOriginator::Extension {
                name: crate::test_extension_name("conn-side-length"),
                query_id: "q-length".to_owned(),
            },
            usage: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: Some(tau_proto::ProviderBackend {
                kind: tau_proto::ProviderBackendKind::ChatCompletions,
                base_url: "https://example.invalid/v1".to_owned(),
                transport: tau_proto::ProviderBackendTransport::HttpSse,
                stale_chain_fallback: false,
            }),
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        })
        .expect("side length response handled");

        let result = frames
            .lock()
            .expect("frames")
            .iter()
            .find_map(|routed| match peel_inner_event(&routed.frame) {
                Some(Event::StartAgentResult(result)) if result.query_id == "q-length" => {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("unsuccessful start-agent result");
        assert_eq!(
            result.error.as_deref(),
            Some("provider failure: unknown"),
            "case {index} ({name}) must return an explicit error, never a successful result"
        );
        if name == "partial prose" {
            assert_eq!(
                result.text, "partial delegated answer",
                "case {index} ({name}) preserves partial prose while remaining unsuccessful"
            );
        } else {
            assert!(
                result.text.is_empty(),
                "case {index} ({name}) must not fabricate assistant text"
            );
        }
        h.shutdown().expect("shutdown");
    }
}

#[test]
fn generic_agent_watch_snapshots_replay_current_state() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let live = connect_test_tool(&mut h, "watch-live");
    h.complete_subscription(
        &crate::test_connection_id("watch-live"),
        Vec::new(),
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_WATCHES_UPDATED,
        )],
    )
    .expect("subscribe");
    drain_watches_updated(&live);

    h.set_agent_watch(
        "watcher",
        "child-b",
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_watch(
        "watcher",
        "child-a",
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_watch(
        "watcher",
        "child-b",
        false,
        tau_proto::AgentWatchUpdateCause::AgentWatchDisable,
    );
    h.set_agent_watch(
        "watcher",
        "child-a",
        false,
        tau_proto::AgentWatchUpdateCause::AgentWatchDisable,
    );

    let snapshots = drain_watches_updated(&live);
    assert_eq!(snapshots.len(), 4);
    assert_eq!(
        snapshots[0].watched_agent_ids,
        vec![crate::parse_agent_id("child-b")]
    );
    assert_eq!(
        snapshots[1].watched_agent_ids,
        vec![
            crate::parse_agent_id("child-a"),
            crate::parse_agent_id("child-b")
        ]
    );
    assert_eq!(
        snapshots[2].watched_agent_ids,
        vec![crate::parse_agent_id("child-a")]
    );
    assert!(snapshots[3].watched_agent_ids.is_empty());

    h.set_agent_watch(
        "watcher",
        "child-a",
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let replay = connect_test_tool(&mut h, "watch-replay");
    h.complete_subscription(
        &crate::test_connection_id("watch-replay"),
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_WATCHES_UPDATED,
        )],
        Vec::new(),
    )
    .expect("subscribe replay");
    let replayed = drain_watches_updated(&replay);
    assert_eq!(replayed.len(), 1);
    assert_eq!(
        replayed[0].cause,
        tau_proto::AgentWatchUpdateCause::SessionSnapshot
    );
    assert_eq!(replayed[0].watcher_id, crate::parse_agent_id("watcher"));
    assert_eq!(
        replayed[0].watched_agent_ids,
        vec![crate::parse_agent_id("child-a")]
    );
    h.shutdown().expect("shutdown");
}
