//! Tests for client requests behavior.

use super::*;

/// UI command rejections remain live-only responses to the initiating UI and
/// never enter publication history or another attached UI's stream.
#[test]
fn ui_command_response_is_requester_only_and_not_logged() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let (requesting_ui_id, mut requesting_ui) = connect_socket_ui(&mut h);
    let (observer_id, mut observer) = connect_socket_ui(&mut h);
    let baseline_seq = h.runtime_io.event_log.next_seq();

    h.handle_client_ui_event(
        &requesting_ui_id,
        Event::UiRoleSelect(tau_proto::UiRoleSelect {
            role: "missing-role".to_owned(),
        }),
    )
    .expect("handle role selection");

    let notice = read_notice(&mut requesting_ui);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Response);
    assert!(notice.message.contains("unknown role"));
    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);

    h.handle_client_ui_event(
        &requesting_ui_id,
        Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
            literal: true,
            session_id: test_session_id("stale-session"),
            text: "rejected prompt".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            agent_id: crate::parse_agent_id("missing-agent"),
            ctx_id: Some("rejected-prompt".to_owned()),
        }),
    )
    .expect("handle prompt rejection");
    let notice = read_notice(&mut requesting_ui);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Response);
    assert!(
        notice
            .message
            .contains("prompt for `stale-session` rejected")
    );
    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);

    h.handle_client_ui_event(
        &requesting_ui_id,
        Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: test_session_id("stale-session"),
            target_agent_id: None,
            agent_prompt_id: None,
        }),
    )
    .expect("handle cancel rejection");
    let notice = read_notice(&mut requesting_ui);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Response);
    assert!(notice.message.contains("stale session"));
    assert_no_message(&mut requesting_ui);
    assert_no_message(&mut observer);
    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("loaded agent")
        .dispatch
        .pending_cancel = Some(crate::agent::PendingCancel {
        requester_client_id: requesting_ui_id.clone(),
        agent_prompt_id: None,
        reason: "cancelled by user".to_owned(),
    });
    h.handle_client_ui_event(
        &observer_id,
        Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id),
            agent_prompt_id: None,
        }),
    )
    .expect("handle duplicate cancel rejection");
    let notice = read_notice(&mut observer);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Response);
    assert!(notice.message.contains("already pending"));
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_cancel
            .as_ref()
            .expect("original cancellation retained")
            .requester_client_id,
        requesting_ui_id
    );
    assert_no_message(&mut requesting_ui);
}

/// Configured extensions are metered and silently denied after legal phase
/// validation but before activation staging.
#[test]
fn tree_request_is_silently_denied_for_configured_extensions() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let ready = connect_ready_configured_extension(
        &mut h,
        "tree-ready-requester",
        "tree-ready-requester",
        tau_proto::ClientKind::Tool,
    );
    ready.lock().expect("ready requester frames").clear();
    let handshaking = connect_handshaking_tool(&mut h, "tree-handshaking-requester");
    let notice_count = h.runtime_io.replayable_harness_notices.len();

    for connection_id in ["tree-ready-requester", "tree-handshaking-requester"] {
        h.handle_extension_message(
            &crate::test_connection_id(connection_id),
            tree_request("s1", None),
        )
        .expect("silently deny configured extension tree request");
        let stats = h.extensions.entries[connection_id]
            .protocol_io
            .cumulative_stats();
        assert_eq!(stats.uplink["message.ui_tree_request"].count, 1);
    }

    assert_eq!(
        h.extensions.entries["tree-ready-requester"].state,
        ExtensionState::Ready
    );
    assert_eq!(
        h.extensions.entries["tree-handshaking-requester"].state,
        ExtensionState::Handshaking
    );
    assert!(
        !h.extensions
            .activation_staging
            .contains_key("tree-handshaking-requester")
    );
    assert!(ready.lock().expect("ready requester frames").is_empty());
    assert!(
        handshaking
            .lock()
            .expect("handshaking requester frames")
            .is_empty()
    );
    assert_eq!(h.runtime_io.replayable_harness_notices.len(), notice_count);
}

/// Tree requests preserve ordinary configured-extension phase validation:
/// pre-Hello requests are metered and then follow runtime protocol-failure
/// isolation instead of the legal-phase silent denial.
#[test]
fn tree_request_preserves_configured_extension_phase_validation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    h.extensions.initial_tool_preflight_complete = true;
    connect_handshaking_tool(&mut h, "tree-spawning-requester");
    h.extensions
        .entries
        .get_mut("tree-spawning-requester")
        .expect("spawning requester")
        .state = ExtensionState::Spawning;
    let notice_count = h.runtime_io.replayable_harness_notices.len();

    h.handle_extension_message(
        &crate::test_connection_id("tree-spawning-requester"),
        tree_request("s1", None),
    )
    .expect("isolate out-of-phase requester");

    let entry = &h.extensions.entries["tree-spawning-requester"];
    assert_eq!(
        entry.protocol_io.cumulative_stats().uplink["message.ui_tree_request"].count,
        1
    );
    assert_eq!(entry.state, ExtensionState::Disconnected);
    assert!(
        h.runtime_io
            .bus
            .connection(&crate::test_connection_id("tree-spawning-requester"))
            .is_none()
    );
    assert_eq!(
        h.runtime_io.replayable_harness_notices.len(),
        notice_count + 1
    );
}

/// Configured extensions are metered and silently denied after phase
/// validation, before activation staging can turn repeated detach attempts into
/// a quota failure.
#[test]
fn detach_request_is_silently_denied_for_configured_extensions() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let ready = connect_ready_configured_extension(
        &mut h,
        "ready-requester",
        "ready-requester",
        tau_proto::ClientKind::Tool,
    );
    ready.lock().expect("ready requester frames").clear();
    let handshaking = connect_handshaking_tool(&mut h, "handshaking-requester");
    let notice_count = h.runtime_io.replayable_harness_notices.len();

    for connection_id in ["ready-requester", "handshaking-requester"] {
        h.handle_extension_message(&crate::test_connection_id(connection_id), detach_request())
            .expect("silently deny configured extension detach");
        let stats = h.extensions.entries[connection_id]
            .protocol_io
            .cumulative_stats();
        assert_eq!(
            stats.uplink["message.ui_detach_request"].count, 1,
            "dedicated detach frame must use the message metering key"
        );
    }

    assert_eq!(
        h.extensions.entries["ready-requester"].state,
        ExtensionState::Ready
    );
    assert_eq!(
        h.extensions.entries["handshaking-requester"].state,
        ExtensionState::Handshaking
    );
    assert!(
        h.runtime_io
            .bus
            .connection(&crate::test_connection_id("ready-requester"))
            .is_some()
    );
    assert!(
        h.runtime_io
            .bus
            .connection(&crate::test_connection_id("handshaking-requester"))
            .is_some()
    );
    assert!(
        !h.extensions
            .activation_staging
            .contains_key("handshaking-requester")
    );
    assert!(ready.lock().expect("ready requester frames").is_empty());
    assert!(
        handshaking
            .lock()
            .expect("handshaking requester frames")
            .is_empty()
    );
    assert_eq!(h.runtime_io.replayable_harness_notices.len(), notice_count);
}

/// Silent configured-extension denial happens only after ordinary phase
/// validation: a detach request before Hello is metered, then follows normal
/// runtime protocol-failure isolation.
#[test]
fn detach_request_preserves_configured_extension_phase_validation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    h.extensions.initial_tool_preflight_complete = true;
    connect_handshaking_tool(&mut h, "spawning-requester");
    h.extensions
        .entries
        .get_mut("spawning-requester")
        .expect("spawning requester")
        .state = ExtensionState::Spawning;
    let notice_count = h.runtime_io.replayable_harness_notices.len();

    h.handle_extension_message(
        &crate::test_connection_id("spawning-requester"),
        detach_request(),
    )
    .expect("isolate out-of-phase requester");

    let entry = &h.extensions.entries["spawning-requester"];
    assert_eq!(
        entry.protocol_io.cumulative_stats().uplink["message.ui_detach_request"].count,
        1
    );
    assert_eq!(entry.state, ExtensionState::Disconnected);
    assert!(
        h.runtime_io
            .bus
            .connection(&crate::test_connection_id("spawning-requester"))
            .is_none()
    );
    assert_eq!(
        h.runtime_io.replayable_harness_notices.len(),
        notice_count + 1
    );
}

/// An attached socket UI receives exactly one multiline tree result while
/// other peers, publication history, and semantic stores see no request or
/// result event. The point-to-point request remains visible in debug JSONL.
#[test]
fn tree_request_returns_one_directed_multiline_notice() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    append_user_message_via_event(&mut h, "s1", "A\u{1b}[2J\rforged\n\u{202e}B\\C\u{9b}D雪");
    append_user_message_via_event(&mut h, "s1", "second tree prompt");
    let (requesting_ui_id, mut requesting_ui) = connect_socket_ui(&mut h);
    let (_observer_id, mut observer) = connect_socket_ui(&mut h);
    let debug_log_path = h
        .enable_debug_log(&td.path().join("debug"))
        .expect("enable debug log");
    let baseline_seq = h.runtime_io.event_log.next_seq();
    let event = HarnessEvent::from_connection_for_test(
        requesting_ui_id,
        tree_request("s1", Some(agent_id.as_str())),
    );
    h.log_event(&event);

    let mut served_clients = 0;
    let mut exit_on_disconnect = false;
    let mut ever_attached = false;
    h.handle_runtime_event(
        event,
        &mut served_clients,
        &mut exit_on_disconnect,
        &mut ever_attached,
    )
    .expect("handle tree request");

    let notice = read_notice(&mut requesting_ui);
    assert_eq!(notice.kind, tau_proto::notice_kind::HARNESS_NOTICE);
    assert_eq!(notice.level, tau_proto::NoticeLevel::Info);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Response);
    assert_eq!(
        notice.message,
        concat!(
            "    0   before first prompt (root)\n",
            r"    1   before prompt  user: A\u{001B}[2J\u{000D}forged \u{202E}B\\C\u{009B}D雪",
            "\n",
            "    2   before prompt  user: second tree prompt",
        )
    );
    assert_eq!(notice.message.lines().count(), 3);
    assert!(
        notice.message.chars().all(|character| {
            character == '\n' || !tau_proto::requires_visible_escape(character)
        })
    );
    assert_no_message(&mut requesting_ui);
    assert_no_message(&mut observer);
    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);

    let debug_lines = std::fs::read_to_string(debug_log_path).expect("read debug log");
    let entries = debug_lines
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("debug JSON"))
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["event_name"], "<message>");
    assert_eq!(entries[0]["event"]["message"], "ui_tree_request");
    assert_eq!(entries[0]["event"]["payload"]["session_id"], "s1");
}

/// Startup intake preserves tree request handling without treating the request
/// as a subscription or publishing its one directed error result.
#[test]
fn tree_request_returns_directed_result_during_startup() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    let baseline_seq = h.runtime_io.event_log.next_seq();

    assert!(
        !h.handle_startup_from_connection(&ui_id, tree_request("s1", None))
            .expect("handle startup tree request")
    );

    let notice = read_notice(&mut ui);
    assert_eq!(notice.message, "tree request ignored: unknown agent");
    assert_no_message(&mut ui);
    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);
}

/// Non-UI sockets, dedicated external-message peers, and embedded UIs cannot
/// inspect agent tree previews or trigger a directed result.
#[test]
fn tree_request_is_silently_denied_for_other_client_origins() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let socket_tool = connect_test_client_with_origin(
        &mut h,
        "tree-socket-tool",
        tau_proto::ClientKind::Tool,
        ConnectionOrigin::Socket,
    );
    let embedded_ui = connect_test_client(&mut h, "tree-embedded-ui", tau_proto::ClientKind::Ui);
    let (external_id, mut external) = connect_socket_ui(&mut h);
    h.handle_client_message(
        &external_id,
        HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name(
                crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME,
            ),
            client_kind: tau_proto::ClientKind::External,
            expected_session_id: None,
            capabilities: Default::default(),
        }),
    )
    .expect("external-agent message hello");
    let baseline_seq = h.runtime_io.event_log.next_seq();

    for connection_id in [
        tau_proto::ConnectionId::parse("tree-socket-tool")
            .expect("test connection id must satisfy the identifier grammar"),
        tau_proto::ConnectionId::parse("tree-embedded-ui")
            .expect("test connection id must satisfy the identifier grammar"),
        external_id,
    ] {
        let mut served_clients = 0;
        let mut exit_on_disconnect = false;
        let mut ever_attached = false;
        h.handle_runtime_event(
            HarnessEvent::from_connection_for_test(connection_id, tree_request("s1", None)),
            &mut served_clients,
            &mut exit_on_disconnect,
            &mut ever_attached,
        )
        .expect("silently deny tree request");
        assert_eq!(served_clients, 0);
    }

    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);
    assert!(socket_tool.lock().expect("socket tool frames").is_empty());
    assert!(embedded_ui.lock().expect("embedded UI frames").is_empty());
    assert_no_message(&mut external);
}

/// An attached socket UI may disable exit-on-disconnect without publishing,
/// delivering, or persisting a bus event. The point-to-point frame remains
/// visible in the local debug JSONL trace.
#[test]
fn detach_request_controls_runtime_without_publication() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let debug_log_path = h
        .enable_debug_log(&td.path().join("debug"))
        .expect("enable debug log");
    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    let (_observer_id, mut observer) = connect_socket_ui(&mut h);
    let baseline_seq = h.runtime_io.event_log.next_seq();
    let event = HarnessEvent::from_connection_for_test(ui_id, detach_request());
    h.log_event(&event);

    let mut served_clients = 0;
    let mut exit_on_disconnect = true;
    let mut ever_attached = false;
    h.handle_runtime_event(
        event,
        &mut served_clients,
        &mut exit_on_disconnect,
        &mut ever_attached,
    )
    .expect("handle detach request");

    assert!(!exit_on_disconnect);
    assert_eq!(served_clients, 0);
    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);
    assert_no_message(&mut ui);
    assert_no_message(&mut observer);

    let lines = std::fs::read_to_string(debug_log_path).expect("read debug log");
    let entries = lines
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("debug JSON"))
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["event_name"], "<message>");
    assert_eq!(entries[0]["event"]["message"], "ui_detach_request");
    assert_eq!(entries[0]["event"]["payload"], serde_json::json!({}));
}

/// Startup gating recognizes detach only from an exact attached socket UI.
/// Socket origin alone must not grant connection-control authority.
#[test]
fn detach_request_controls_startup_only_for_attached_socket_ui() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    connect_test_client_with_origin(
        &mut h,
        "socket-tool",
        tau_proto::ClientKind::Tool,
        ConnectionOrigin::Socket,
    );

    assert!(
        !h.handle_startup_from_connection(
            &crate::test_connection_id("socket-tool"),
            detach_request()
        )
        .expect("deny socket tool detach")
    );
    assert!(!h.ui_runtime.startup_detach_requested);

    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    assert!(
        !h.handle_startup_from_connection(&ui_id, detach_request())
            .expect("handle attached UI detach")
    );
    assert!(h.ui_runtime.startup_detach_requested);
    assert_no_message(&mut ui);
}

/// Non-UI sockets, dedicated external-message peers, and embedded UIs cannot
/// mutate the runtime exit-on-disconnect control.
#[test]
fn detach_request_is_silently_denied_for_other_client_origins() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let socket_tool = connect_test_client_with_origin(
        &mut h,
        "socket-tool",
        tau_proto::ClientKind::Tool,
        ConnectionOrigin::Socket,
    );
    let embedded_ui = connect_test_client(&mut h, "embedded-ui", tau_proto::ClientKind::Ui);
    let (external_id, mut external) = connect_socket_ui(&mut h);
    h.handle_client_message(
        &external_id,
        HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name(
                crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME,
            ),
            client_kind: tau_proto::ClientKind::External,
            expected_session_id: None,
            capabilities: Default::default(),
        }),
    )
    .expect("external-agent message hello");
    let baseline_seq = h.runtime_io.event_log.next_seq();

    for connection_id in [
        tau_proto::ConnectionId::parse("socket-tool")
            .expect("test connection id must satisfy the identifier grammar"),
        tau_proto::ConnectionId::parse("embedded-ui")
            .expect("test connection id must satisfy the identifier grammar"),
        external_id,
    ] {
        let mut served_clients = 0;
        let mut exit_on_disconnect = true;
        let mut ever_attached = false;
        h.handle_runtime_event(
            HarnessEvent::from_connection_for_test(connection_id, detach_request()),
            &mut served_clients,
            &mut exit_on_disconnect,
            &mut ever_attached,
        )
        .expect("silently deny detach request");
        assert!(exit_on_disconnect);
        assert_eq!(served_clients, 0);
    }

    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);
    assert!(socket_tool.lock().expect("socket tool frames").is_empty());
    assert!(embedded_ui.lock().expect("embedded UI frames").is_empty());
    assert_no_message(&mut external);
}

/// A socket client cannot claim report or canonical message publication
/// authority.
#[test]
fn socket_client_cannot_emit_message_reports_or_canonical_facts() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let client_id = "ui";
    connect_test_client(&mut h, client_id, tau_proto::ClientKind::Ui);
    let fact = Event::MessageDelivered(tau_proto::MessageDelivered {
        publisher_extension_id: tau_proto::MessagePublisherId::parse("forged")
            .expect("canonical publisher"),
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
    });

    h.handle_client_event_inner(&crate::test_connection_id(client_id), fact)
        .expect("client intake");
    h.handle_client_event_inner(
        &crate::test_connection_id(client_id),
        Event::MessageDeliveredReported(tau_proto::MessageDelivered::new(
            tau_proto::RawMessagePublisherId::new("forged"),
            tau_proto::MessageAgentTarget::new("missing-agent"),
            tau_proto::MessageFactId::new("m2"),
            tau_proto::MessageParty {
                stable_id: "u1".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            None,
            "hello",
        )),
    )
    .expect("client report intake");

    assert!(!event_log_contains_source_event(&h, client_id, |event| {
        matches!(
            event,
            Event::MessageDelivered(_) | Event::MessageDeliveredReported(_)
        )
    }));
}
