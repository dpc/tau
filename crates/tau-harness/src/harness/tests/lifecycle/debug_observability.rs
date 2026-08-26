//! Tests for debug observability behavior.

use super::*;

/// Extension input frames should be counted through the normal harness message
/// intake path before the debug command reads the live extension's meter.
#[test]
fn debug_event_stats_request_reports_recorded_extension_input() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    connect_handshaking_tool(&mut h, "std-shell");
    h.extensions
        .entries
        .get_mut("std-shell")
        .expect("extension")
        .state = ExtensionState::Spawning;

    h.handle_extension_event(
        "std-shell",
        TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("std-shell"),
            client_kind: tau_proto::ClientKind::Tool,
            expected_session_id: None,
            capabilities: Default::default(),
        })),
    )
    .expect("extension hello");
    h.handle_client_message(&ui_id, debug_event_stats_request("std-shell"))
        .expect("request stats");

    let notice = read_notice(&mut ui);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Response);
    assert!(notice.message.contains("message.hello:"));
    assert!(notice.message.contains("extension -> harness:"));
}

/// Configured extensions cannot round-trip the client-only debug request to
/// obtain another extension's counters.
#[test]
fn debug_event_stats_request_is_ignored_from_configured_extensions() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let debug_log_path = h
        .enable_debug_log(&td.path().join("debug"))
        .expect("enable debug log");
    let requester = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Tool,
    );
    requester.lock().expect("requester frames").clear();
    let meter = tau_client::ProtocolIoMeter::default();
    meter.record_bytes(
        tau_client::ProtocolIoDirection::Uplink,
        "secret.extension_event".to_owned(),
        Some(128),
    );
    insert_extension_entry_with_meter(
        &mut h,
        "ext-secret",
        "secret-ext",
        ExtensionState::Ready,
        meter,
    );

    let notice_count = h.runtime_io.replayable_harness_notices.len();
    let event = HarnessEvent::from_connection_for_test(
        crate::test_connection_id("requester"),
        debug_event_stats_request("secret-ext"),
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
    .expect("ignore client-only request through runtime router");

    assert!(
        requester.lock().expect("requester frames").is_empty(),
        "extension must not receive counter data or a UI diagnostic"
    );
    assert_eq!(
        h.extensions.entries["requester"].state,
        ExtensionState::Ready,
        "silently denied requests must not disconnect the extension"
    );
    assert_eq!(
        h.runtime_io.replayable_harness_notices.len(),
        notice_count,
        "silently denied requests must not publish a replayable warning"
    );
    assert!(
        std::fs::read_to_string(debug_log_path)
            .expect("read debug log")
            .is_empty(),
        "the request and denial must remain absent from debug JSONL"
    );
}

/// A configured extension's request is silently denied after phase validation
/// and metering, before activation staging can turn repetitions into a quota
/// warning, disconnect, or required-startup failure.
#[test]
fn debug_event_stats_request_is_not_staged_for_handshaking_extensions() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let debug_log_path = h
        .enable_debug_log(&td.path().join("debug"))
        .expect("enable debug log");
    let requester = connect_handshaking_tool(&mut h, "requester");
    let notice_count = h.runtime_io.replayable_harness_notices.len();
    let event = HarnessEvent::from_connection_for_test(
        crate::test_connection_id("requester"),
        debug_event_stats_request("secret-ext"),
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
    .expect("silently deny request through runtime router");

    assert!(requester.lock().expect("requester frames").is_empty());
    assert_eq!(
        h.extensions.entries["requester"].state,
        ExtensionState::Handshaking
    );
    assert!(
        h.runtime_io
            .bus
            .connection(&crate::test_connection_id("requester"))
            .is_some()
    );
    assert!(
        !h.extensions.activation_staging.contains_key("requester"),
        "denied request must not consume activation quota"
    );
    assert_eq!(h.runtime_io.replayable_harness_notices.len(), notice_count);
    assert!(
        std::fs::read_to_string(debug_log_path)
            .expect("read debug log")
            .is_empty()
    );
}

/// Startup intake preserves the existing directed no-live result without
/// publishing, staging, or treating the request as a subscription.
#[test]
fn debug_event_stats_request_reports_no_live_extension_during_startup() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let (ui_id, mut ui) = connect_socket_ui(&mut h);

    assert!(
        !h.handle_startup_from_connection(&ui_id, debug_event_stats_request("missing-extension"),)
            .expect("request stats through startup router")
    );

    let notice = read_notice(&mut ui);
    assert_eq!(notice.kind, tau_proto::notice_kind::UI_COMMAND_ERROR);
    assert!(
        notice
            .message
            .contains("no live extension named `missing-extension`")
    );
    assert_no_message(&mut ui);
}

/// A disconnected extension entry should not satisfy a debug stats request when
/// a newer live entry with the same configured name exists; this prevents
/// respawn/disconnect churn from reporting stale meters as current.
#[test]
fn debug_event_stats_request_ignores_disconnected_extension_entry() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    let stale_meter = tau_client::ProtocolIoMeter::default();
    stale_meter.record_bytes(
        tau_client::ProtocolIoDirection::Uplink,
        "stale.event".to_owned(),
        Some(999),
    );
    let live_meter = tau_client::ProtocolIoMeter::default();
    live_meter.record_bytes(
        tau_client::ProtocolIoDirection::Downlink,
        "live.event".to_owned(),
        Some(64),
    );
    insert_extension_entry_with_meter(
        &mut h,
        "old-shell",
        "std-shell",
        ExtensionState::Disconnected,
        stale_meter,
    );
    insert_extension_entry_with_meter(
        &mut h,
        "new-shell",
        "std-shell",
        ExtensionState::Ready,
        live_meter,
    );

    h.handle_client_message(&ui_id, debug_event_stats_request("std-shell"))
        .expect("request stats");

    let notice = read_notice(&mut ui);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Response);
    assert!(notice.message.contains("live.event: 64B count=1"));
    assert!(!notice.message.contains("stale.event"));
}

/// Ambiguous live configured extension names should produce a directed error
/// instead of choosing one meter arbitrarily.
#[test]
fn debug_event_stats_request_rejects_ambiguous_live_extension_name() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    insert_extension_entry_with_meter(
        &mut h,
        "std-shell-a",
        "std-shell",
        ExtensionState::Ready,
        tau_client::ProtocolIoMeter::default(),
    );
    insert_extension_entry_with_meter(
        &mut h,
        "std-shell-b",
        "std-shell",
        ExtensionState::Ready,
        tau_client::ProtocolIoMeter::default(),
    );

    h.handle_client_message(&ui_id, debug_event_stats_request("std-shell"))
        .expect("request stats");

    let notice = read_notice(&mut ui);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Response);
    assert!(
        notice
            .message
            .contains("extension name `std-shell` matched 2 live connections")
    );
}

/// The synchronous harness fault seam preserves rollback-poison lifecycle
/// behavior; direct writer tests own production singleton-poison coverage.
#[test]
fn synchronous_debug_log_poison_prevents_reenable() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut harness = echo_harness(td.path()).expect("harness");
    harness
        .runtime_io
        .debug_log
        .as_mut()
        .expect("durable harness debug log")
        .inject_rollback_failure();

    harness.log_event(&HarnessEvent::Disconnected {
        connection_id: tau_proto::ConnectionId::parse("conn-1")
            .expect("test connection id must satisfy the identifier grammar"),
    });

    assert!(harness.runtime_io.debug_log_poisoned);
    assert!(harness.runtime_io.debug_log.is_none());
    let replacement_dir = td.path().join("replacement-session");
    let error = harness
        .enable_debug_log(&replacement_dir)
        .expect_err("process-lifetime poison rejects replacement log");
    assert!(error.to_string().contains("append disabled"));
    assert!(
        !replacement_dir.exists(),
        "poison must reject replacement before touching its path"
    );
}

/// A UI debug event-stats request should receive a directed, non-persisted
/// notice for the requested live extension only; other UIs must not see the
/// response merely because they are connected.
#[test]
fn debug_event_stats_request_is_directed_to_requesting_ui() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let (requesting_ui_id, mut requesting_ui) = connect_socket_ui(&mut h);
    let (_other_ui_id, mut other_ui) = connect_socket_ui(&mut h);
    let meter = tau_client::ProtocolIoMeter::default();
    meter.record_bytes(
        tau_client::ProtocolIoDirection::Uplink,
        "tool.started".to_owned(),
        Some(42),
    );
    insert_extension_entry_with_meter(
        &mut h,
        "ext-shell",
        "std-shell",
        ExtensionState::Ready,
        meter,
    );

    let mut served_clients = 0;
    let mut exit_on_disconnect = false;
    let mut ever_attached = false;
    h.handle_runtime_event(
        HarnessEvent::from_connection_for_test(
            requesting_ui_id,
            debug_event_stats_request("std-shell"),
        ),
        &mut served_clients,
        &mut exit_on_disconnect,
        &mut ever_attached,
    )
    .expect("request stats through runtime router");

    let notice = read_notice(&mut requesting_ui);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Response);
    assert!(
        notice
            .message
            .contains("Extension `std-shell` protocol I/O cumulative stats")
    );
    assert!(
        notice
            .message
            .contains("extension -> harness: 42B in 1 frame(s)")
    );
    assert!(notice.message.contains("tool.started: 42B count=1"));
    assert_no_message(&mut other_ui);
}

/// Non-socket test/embedded connections are not authorized for extension
/// protocol stats because those counters expose privileged operational
/// metadata outside normal subscription visibility.
#[test]
fn debug_event_stats_request_rejects_unauthorized_ui_origin() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    let other_ui = connect_test_client(&mut h, "other-ui", tau_proto::ClientKind::Ui);
    let meter = tau_client::ProtocolIoMeter::default();
    meter.record_bytes(
        tau_client::ProtocolIoDirection::Uplink,
        "secret.extension_event".to_owned(),
        Some(128),
    );
    insert_extension_entry_with_meter(
        &mut h,
        "ext-secret",
        "secret-ext",
        ExtensionState::Ready,
        meter,
    );

    let mut served_clients = 0;
    let mut exit_on_disconnect = false;
    let mut ever_attached = false;
    h.handle_runtime_event(
        HarnessEvent::from_connection_for_test(
            crate::test_connection_id("ui"),
            debug_event_stats_request("secret-ext"),
        ),
        &mut served_clients,
        &mut exit_on_disconnect,
        &mut ever_attached,
    )
    .expect("request stats through runtime router");

    let frames = ui.lock().expect("UI frames");
    assert_eq!(frames.len(), 1, "denial must produce exactly one frame");
    let Some(Event::HarnessNotice(notice)) = peel_inner_event(&frames[0].frame) else {
        panic!("expected one directed harness notice: {frames:?}");
    };
    assert_eq!(notice.kind, tau_proto::notice_kind::UI_COMMAND_ERROR);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Response);
    assert_eq!(
        notice.message,
        "extension event stats are only available to attached local UIs"
    );
    assert!(
        other_ui.lock().expect("other UI frames").is_empty(),
        "denial must not leak to another peer"
    );
}

/// A dedicated external-message peer cannot reach the UI diagnostics handler or
/// observe extension protocol counters.
#[test]
fn debug_event_stats_request_rejects_dedicated_external_peer_without_leaking_counters() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let (client_id, mut client) = connect_socket_ui(&mut h);
    let (_other_ui_id, mut other_ui) = connect_socket_ui(&mut h);
    h.handle_client_message(
        &client_id,
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
    let meter = tau_client::ProtocolIoMeter::default();
    meter.record_bytes(
        tau_client::ProtocolIoDirection::Uplink,
        "secret.extension_event".to_owned(),
        Some(128),
    );
    insert_extension_entry_with_meter(
        &mut h,
        "ext-secret",
        "secret-ext",
        ExtensionState::Ready,
        meter,
    );

    h.handle_client_message(&client_id, debug_event_stats_request("secret-ext"))
        .expect("request stats");

    assert_no_message(&mut client);
    assert_no_message(&mut other_ui);
}
