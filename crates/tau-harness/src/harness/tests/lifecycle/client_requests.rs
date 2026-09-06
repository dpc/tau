//! Tests for client requests behavior.

use super::*;

/// Build a correlated quit control for the focused lifetime oracles.
fn quit_request(detach: bool) -> HarnessInputMessage {
    HarnessInputMessage::UiQuitRequest(tau_proto::UiQuitRequest {
        request_id: "quit-1".to_owned(),
        detach,
    })
}

/// Expected directed reply, including the original request correlation.
fn quit_reply(disposition: tau_proto::UiQuitDisposition) -> HarnessOutputMessage {
    HarnessOutputMessage::UiQuitResult(tau_proto::UiQuitResult {
        request_id: "quit-1".to_owned(),
        disposition,
    })
}

/// Expected read-only current-state projection for one UI's ordinary quit.
fn quit_projection(disposition: tau_proto::UiQuitDisposition) -> HarnessOutputMessage {
    HarnessOutputMessage::UiQuitDispositionChanged(tau_proto::UiQuitDispositionChanged {
        disposition,
    })
}

/// A UI's projected ordinary-quit outcome changes when another participating UI
/// arrives or leaves, while the harness remains the sole lifecycle authority.
#[test]
fn quit_projection_tracks_other_ui_admission_and_departure() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    h.ui_runtime.exit_on_disconnect = true;

    let (first_id, mut first) = connect_socket_ui(&mut h);
    h.publish_ui_quit_dispositions();
    assert_eq!(
        first.read_message().expect("initial projection"),
        Some(quit_projection(tau_proto::UiQuitDisposition::Terminating))
    );

    let (second_id, mut second) = connect_socket_ui(&mut h);
    h.publish_ui_quit_dispositions();
    for ui in [&mut first, &mut second] {
        assert_eq!(
            ui.read_message().expect("joined projection"),
            Some(quit_projection(tau_proto::UiQuitDisposition::Detached))
        );
    }

    h.handle_runtime_disconnect(second_id, &mut 0)
        .expect("other UI EOF");
    assert_eq!(
        first.read_message().expect("post-EOF projection"),
        Some(quit_projection(tau_proto::UiQuitDisposition::Terminating))
    );
    assert!(!h.ui_runtime.quitting_uis.contains(&first_id));
}

/// Acknowledge a quitter before EOF, then refresh remaining UI help from the
/// same state transition; explicit detach clears the policy for future quits.
#[test]
fn quit_projection_tracks_quitting_and_detach_policy_clear() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    h.ui_runtime.exit_on_disconnect = true;

    let (first_id, mut first) = connect_socket_ui(&mut h);
    let (second_id, mut second) = connect_socket_ui(&mut h);
    h.publish_ui_quit_dispositions();
    let _ = first.read_message().expect("first initial projection");
    let _ = second.read_message().expect("second initial projection");

    h.handle_ui_quit_request(&second_id, &quit_request(false));
    assert_eq!(
        second.read_message().expect("quitter result"),
        Some(quit_reply(tau_proto::UiQuitDisposition::Detached))
    );
    assert_eq!(
        first.read_message().expect("quitting projection"),
        Some(quit_projection(tau_proto::UiQuitDisposition::Terminating))
    );
    assert!(h.ui_runtime.quitting_uis.contains(&second_id));

    let (third_id, mut third) = connect_socket_ui(&mut h);
    h.publish_ui_quit_dispositions();
    for ui in [&mut first, &mut third] {
        assert_eq!(
            ui.read_message().expect("replacement UI projection"),
            Some(quit_projection(tau_proto::UiQuitDisposition::Detached))
        );
    }

    h.handle_ui_quit_request(&third_id, &quit_request(true));
    assert_eq!(
        third.read_message().expect("detach result"),
        Some(quit_reply(tau_proto::UiQuitDisposition::Detached))
    );
    assert_eq!(
        first.read_message().expect("detach projection"),
        Some(quit_projection(tau_proto::UiQuitDisposition::Detached))
    );
    assert!(!h.ui_runtime.exit_on_disconnect);
    assert!(!h.ui_runtime.quitting_uis.contains(&first_id));
}

/// Exact socket admission sends the initial current-quit projection only after
/// the UI completes Hello and becomes a lifetime participant.
#[test]
fn quit_projection_follows_exact_ui_admission() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    h.ui_runtime.exit_on_disconnect = true;
    h.session_runtime.exact_socket_session_required = true;
    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    let mut served_clients = 0;

    h.handle_runtime_event(
        HarnessEvent::from_connection_for_test(
            ui_id,
            HarnessInputMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: crate::test_extension_name("projection-ui"),
                client_kind: tau_proto::ClientKind::Ui,
                expected_session_id: Some(test_session_id("s1")),
                capabilities: Vec::new(),
            }),
        ),
        &mut served_clients,
    )
    .expect("admit UI");

    assert!(matches!(
        ui.read_message().expect("admission"),
        Some(HarnessOutputMessage::SessionAccepted(accepted))
            if accepted.session_id == test_session_id("s1")
    ));
    assert_eq!(
        ui.read_message().expect("initial projection"),
        Some(quit_projection(tau_proto::UiQuitDisposition::Terminating))
    );
    assert_eq!(served_clients, 0);
}

/// The initial startup UI receives the same projection after its Hello, before
/// its later Subscribe completes the startup handshake.
#[test]
fn quit_projection_follows_initial_ui_hello() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    h.ui_runtime.exit_on_disconnect = true;
    let (ui_id, mut ui) = connect_socket_ui(&mut h);

    assert!(
        !h.handle_startup_from_connection(
            &ui_id,
            HarnessInputMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: crate::test_extension_name("initial-projection-ui"),
                client_kind: tau_proto::ClientKind::Ui,
                expected_session_id: Some(test_session_id("s1")),
                capabilities: Vec::new(),
            }),
        )
        .expect("accept initial UI hello")
    );

    assert!(matches!(
        ui.read_message().expect("admission"),
        Some(HarnessOutputMessage::SessionAccepted(accepted))
            if accepted.session_id == test_session_id("s1")
    ));
    assert_eq!(
        ui.read_message().expect("initial projection"),
        Some(quit_projection(tau_proto::UiQuitDisposition::Terminating))
    );
}

/// The auto-shutdown flag is dormant before the first admitted UI, ignores
/// partial socket admission, and survives the departure of a non-final UI.
#[test]
fn auto_shutdown_waits_for_first_and_last_authenticated_ui() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    h.ui_runtime.exit_on_disconnect = true;
    h.session_runtime.exact_socket_session_required = true;
    h.update_ui_disconnect_policy();
    assert!(!h.ui_runtime.ever_attached);
    assert!(!h.ui_runtime.shutdown_requested);

    let (pending, _peer) = UnixStream::pair().expect("pending socket");
    let pending_id = h.accept_client(pending).expect("accept pending socket");
    h.update_ui_disconnect_policy();
    assert!(!h.ui_runtime.ever_attached);
    h.handle_disconnect(&pending_id);

    let (creator, _creator_reader) = connect_socket_ui(&mut h);
    let (attached, _attached_reader) = connect_socket_ui(&mut h);
    // This focused loop oracle begins after successful exact Hello admission.
    h.ui_runtime.pending_socket_admission.remove(&creator);
    h.ui_runtime.pending_socket_admission.remove(&attached);
    h.update_ui_disconnect_policy();
    assert!(h.ui_runtime.ever_attached);
    h.handle_runtime_disconnect(creator, &mut 0)
        .expect("creator EOF");
    h.update_ui_disconnect_policy();
    assert!(!h.ui_runtime.shutdown_requested);
    h.handle_runtime_disconnect(attached, &mut 0)
        .expect("last UI EOF");
    h.update_ui_disconnect_policy();
    assert!(h.ui_runtime.shutdown_requested);
}

/// Detach clears policy before its directed acknowledgment, without publication
/// or replay mutation. Reconnecting and quitting cannot rearm the flag.
#[test]
fn explicit_detach_is_acknowledged_and_sticky_across_reconnection() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    h.ui_runtime.exit_on_disconnect = true;
    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    let baseline_seq = h.runtime_io.event_log.next_seq();
    h.handle_runtime_event(
        HarnessEvent::from_connection_for_test(ui_id.clone(), quit_request(true)),
        &mut 0,
    )
    .expect("detach");
    assert!(!h.ui_runtime.exit_on_disconnect);
    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);
    assert_eq!(
        ui.read_message().expect("reply"),
        Some(quit_reply(tau_proto::UiQuitDisposition::Detached))
    );
    h.handle_runtime_disconnect(ui_id, &mut 0)
        .expect("disconnect");
    let (reattached_id, mut reattached) = connect_socket_ui(&mut h);
    let reconnect_seq = h.runtime_io.event_log.next_seq();
    h.handle_runtime_event(
        HarnessEvent::from_connection_for_test(reattached_id.clone(), quit_request(false)),
        &mut 0,
    )
    .expect("ordinary quit");
    assert_eq!(h.runtime_io.event_log.next_seq(), reconnect_seq);
    assert_eq!(
        reattached.read_message().expect("reply"),
        Some(quit_reply(tau_proto::UiQuitDisposition::Detached))
    );
    h.handle_runtime_disconnect(reattached_id, &mut 0)
        .expect("disconnect");
    h.update_ui_disconnect_policy();
    assert!(!h.ui_runtime.shutdown_requested);
}

/// Concurrent quits release lifetime participation at the serialized decision,
/// not eventual EOF, so the final quitter receives Terminating rather than both
/// UIs falsely receiving Detached.
#[test]
fn final_quit_selects_canonical_shutdown_before_transport_eof() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    h.ui_runtime.exit_on_disconnect = true;
    let (first_id, mut first) = connect_socket_ui(&mut h);
    let (last_id, mut last) = connect_socket_ui(&mut h);
    let request = quit_request(false);
    h.handle_ui_quit_request(&first_id, &request);
    assert_eq!(
        first.read_message().expect("first reply"),
        Some(quit_reply(tau_proto::UiQuitDisposition::Detached))
    );
    assert_eq!(
        last.read_message().expect("remaining UI projection"),
        Some(quit_projection(tau_proto::UiQuitDisposition::Terminating))
    );
    h.handle_ui_quit_request(&last_id, &request);
    assert_eq!(
        last.read_message().expect("last reply"),
        Some(quit_reply(tau_proto::UiQuitDisposition::Terminating))
    );
    assert!(h.ui_runtime.shutdown_requested);
    assert_eq!(h.ui_runtime.client_writers.len(), 2);
    // A late detach cannot reverse a shutdown decision.
    h.handle_ui_quit_request(&first_id, &quit_request(true));
    assert!(h.ui_runtime.shutdown_requested);
    h.shutdown().expect("canonical shutdown");
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::SessionShutdown(_)))
            .count(),
        1
    );
}

/// Startup detach has the same authority and ordering as runtime detach, and
/// acknowledged UI loss must not turn a deliberate background launch into a
/// fatal startup-handshake failure.
#[test]
fn startup_detach_is_authoritative_before_disconnect() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    h.ui_runtime.exit_on_disconnect = true;
    connect_test_client_with_origin(
        &mut h,
        "tool-detach",
        tau_proto::ClientKind::Tool,
        ConnectionOrigin::Socket,
    );
    let tool_id = crate::test_connection_id("tool-detach");
    let request = quit_request(true);
    h.handle_startup_from_connection(&tool_id, request.clone())
        .expect("deny non-UI");
    assert!(h.ui_runtime.exit_on_disconnect);
    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    h.handle_startup_from_connection(&ui_id, request)
        .expect("startup detach");
    assert!(!h.ui_runtime.exit_on_disconnect);
    assert_eq!(
        ui.read_message().expect("reply"),
        Some(quit_reply(tau_proto::UiQuitDisposition::Detached))
    );
    h.handle_startup_disconnect(&ui_id)
        .expect("acknowledged startup departure");
    h.update_ui_disconnect_policy();
    assert!(!h.ui_runtime.shutdown_requested);
}

/// Headless sessions never acquire automatic-shutdown policy merely because
/// a UI connects and then requests ordinary quit.
#[test]
fn headless_session_survives_attached_quit() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    h.handle_ui_quit_request(&ui_id, &quit_request(false));
    assert_eq!(
        ui.read_message().expect("reply"),
        Some(quit_reply(tau_proto::UiQuitDisposition::Detached))
    );
    h.handle_runtime_disconnect(ui_id, &mut 0)
        .expect("disconnect");
    h.update_ui_disconnect_policy();
    assert!(!h.ui_runtime.shutdown_requested);
}

/// A frame that fails decoding because it uses a removed session-control
/// encoding retires only that UI connection and publishes no session facts.
#[test]
fn removed_session_control_decode_failure_isolates_one_ui() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    let (rejected_id, _rejected) = connect_socket_ui(&mut h);
    let (surviving_id, _surviving) = connect_socket_ui(&mut h);
    let baseline_lifecycle = event_log_events(&h)
        .into_iter()
        .filter(|event| matches!(event, Event::SessionStarted(_) | Event::SessionShutdown(_)))
        .count();
    let mut served_clients = 0;

    h.handle_runtime_event(
        HarnessEvent::ReadFailed {
            connection_id: rejected_id.clone(),
            error: "removed ui.switch_session/ui_detach_request encoding".to_owned(),
        },
        &mut served_clients,
    )
    .expect("isolate decode failure");

    assert_eq!(served_clients, 1);
    assert!(!h.ui_runtime.client_writers.contains_key(&rejected_id));
    assert!(h.ui_runtime.client_writers.contains_key(&surviving_id));
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| {
                matches!(event, Event::SessionStarted(_) | Event::SessionShutdown(_))
            })
            .count(),
        baseline_lifecycle
    );
    assert!(!h.ui_runtime.shutdown_requested);
    h.shutdown().expect("shutdown");
}

/// Exact runtime probes are admitted only for current-session diagnostics.
/// Semantic requests close the probe without consuming completed-client budget
/// or mutating daemon lifecycle state.
#[test]
fn runtime_probe_is_quarantined_and_not_counted_as_a_served_ui() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    h.session_runtime.exact_socket_session_required = true;
    let (probe_id, mut probe) = connect_socket_ui(&mut h);
    let mut served_clients = 0;

    h.handle_runtime_event(
        HarnessEvent::from_connection_for_test(
            probe_id.clone(),
            HarnessInputMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: tau_proto::ExtensionName::parse("tau-runtime-probe")
                    .expect("probe name"),
                client_kind: tau_proto::ClientKind::Ui,
                expected_session_id: Some(test_session_id("s1")),
                capabilities: Vec::new(),
            }),
        ),
        &mut served_clients,
    )
    .expect("admit runtime probe");
    assert!(matches!(
        probe.read_message().expect("read acceptance"),
        Some(HarnessOutputMessage::SessionAccepted(accepted))
            if accepted.session_id == test_session_id("s1")
    ));
    assert!(h.ui_runtime.runtime_probe_peers.contains(&probe_id));

    h.handle_runtime_event(
        HarnessEvent::from_connection_for_test(probe_id.clone(), shutdown_request()),
        &mut served_clients,
    )
    .expect("quarantine semantic request");

    assert_eq!(served_clients, 0);
    assert!(!h.ui_runtime.shutdown_requested);
    assert!(!h.ui_runtime.client_writers.contains_key(&probe_id));
    assert!(!h.ui_runtime.runtime_probe_peers.contains(&probe_id));
    h.shutdown().expect("shutdown");
}

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
    h.handle_runtime_event(event, &mut served_clients)
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
        h.handle_runtime_event(
            HarnessEvent::from_connection_for_test(connection_id, tree_request("s1", None)),
            &mut served_clients,
        )
        .expect("silently deny tree request");
        assert_eq!(served_clients, 0);
    }

    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);
    assert!(socket_tool.lock().expect("socket tool frames").is_empty());
    assert!(embedded_ui.lock().expect("embedded UI frames").is_empty());
    assert_no_message(&mut external);
}

/// An attached socket UI may request unconditional canonical shutdown without
/// mutating disconnect policy or publishing the request as a bus event.
#[test]
fn shutdown_request_queues_canonical_shutdown_without_publication() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    let (observer_id, mut observer) = connect_socket_ui(&mut h);
    for connection_id in [&ui_id, &observer_id] {
        h.runtime_io
            .bus
            .set_subscriptions(
                connection_id,
                Vec::new(),
                vec![tau_proto::EventSelector::Exact(
                    tau_proto::EventName::SESSION_SHUTDOWN,
                )],
            )
            .expect("subscribe to session shutdown");
    }
    let baseline_seq = h.runtime_io.event_log.next_seq();
    let mut served_clients = 0;

    h.handle_runtime_event(
        HarnessEvent::from_connection_for_test(ui_id, shutdown_request()),
        &mut served_clients,
    )
    .expect("handle shutdown request");

    assert_eq!(served_clients, 0);
    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);
    for reader in [&mut ui, &mut observer] {
        assert_eq!(
            reader.read_message().expect("read quit projection"),
            Some(quit_projection(tau_proto::UiQuitDisposition::Terminating))
        );
    }
    assert!(h.ui_runtime.shutdown_requested);

    h.shutdown().expect("canonical shutdown");
    for reader in [&mut ui, &mut observer] {
        let message = reader
            .read_message()
            .expect("read session shutdown")
            .expect("session shutdown frame");
        assert!(matches!(
            peel_inner_event(&message),
            Some(Event::SessionShutdown(shutdown)) if shutdown.session_id.as_str() == "s1"
        ));
    }
}

/// Socket origin without attached-UI identity cannot request shutdown.
#[test]
fn shutdown_request_is_silently_denied_for_other_client_origins() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    connect_test_client_with_origin(
        &mut h,
        "socket-tool-shutdown",
        tau_proto::ClientKind::Tool,
        ConnectionOrigin::Socket,
    );
    let connection_id = crate::test_connection_id("socket-tool-shutdown");
    let baseline_seq = h.runtime_io.event_log.next_seq();
    let mut served_clients = 0;
    h.ui_runtime.exit_on_disconnect = true;

    h.handle_runtime_event(
        HarnessEvent::from_connection_for_test(connection_id.clone(), shutdown_request()),
        &mut served_clients,
    )
    .expect("silently deny shutdown request");
    h.handle_runtime_event(
        HarnessEvent::from_connection_for_test(connection_id, quit_request(true)),
        &mut served_clients,
    )
    .expect("silently deny detach request");
    assert!(h.ui_runtime.exit_on_disconnect);

    assert!(!h.ui_runtime.shutdown_requested);
    assert_eq!(served_clients, 0);
    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);
    assert!(matches!(
        h.runtime_io.rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
}

/// Startup gating retains an authorized UI shutdown request for the runtime
/// loop while denying a socket peer without attached-UI identity.
#[test]
fn shutdown_request_controls_startup_only_for_attached_socket_ui() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    connect_test_client_with_origin(
        &mut h,
        "socket-tool-startup-shutdown",
        tau_proto::ClientKind::Tool,
        ConnectionOrigin::Socket,
    );
    let tool_id = crate::test_connection_id("socket-tool-startup-shutdown");
    assert!(
        !h.handle_startup_from_connection(&tool_id, shutdown_request())
            .expect("silently deny tool shutdown")
    );
    assert!(!h.ui_runtime.shutdown_requested);

    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    assert!(
        !h.handle_startup_from_connection(&ui_id, shutdown_request())
            .expect("retain UI shutdown")
    );
    assert!(h.ui_runtime.shutdown_requested);
    assert_eq!(
        ui.read_message().expect("read quit projection"),
        Some(quit_projection(tau_proto::UiQuitDisposition::Terminating))
    );
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
