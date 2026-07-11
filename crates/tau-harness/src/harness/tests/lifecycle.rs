use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::ErrorKind;

use super::*;
use crate::agent::PendingPrompt;
use crate::extension::{ExtensionConnectCommand, ExtensionEntry, ExtensionState, spawn_in_process};
use crate::harness::{
    PendingTool, PromptFragmentSource, extension_disconnected_tool_call_error_message,
    prompt_snapshot_tool_error_message, tool_available_again_notice_prompt,
    tool_unavailable_notice_prompt, unavailable_tool_error_message, validate_protocol_version,
};

fn context_text(item: &ContextItem) -> Option<&str> {
    match item {
        ContextItem::Message(message) => message.content.first().map(|part| match part {
            ContentPart::Text { text } => text.as_str(),
        }),
        ContextItem::ToolResult(result) => match &result.output.raw {
            CborValue::Text(text) => Some(text.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn prompt_has_tool(prompt: &AgentPromptCreated, name: &str) -> bool {
    prompt.tools.iter().any(|tool| tool.name == name)
}

fn context_text_count(prompt: &AgentPromptCreated, text: &str) -> usize {
    prompt
        .context
        .flatten()
        .iter()
        .filter(|item| context_text(item) == Some(text))
        .count()
}

fn agent_prompt_text_count(h: &Harness, text: &str) -> usize {
    loaded_agent_events(h, "s1")
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::AgentPromptSubmitted(prompt)
                    if prompt.message_class.is_internal() && prompt.text == text
            )
        })
        .count()
}

fn event_log_contains_source_event(
    h: &Harness,
    source: &str,
    mut predicate: impl FnMut(&Event) -> bool,
) -> bool {
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if entry.source.as_deref() == Some(source) && predicate(&entry.event) {
            return true;
        }
    }
    false
}

fn prompt_context_contains(prompt: &AgentPromptCreated, needle: &str) -> bool {
    prompt
        .context
        .flatten()
        .iter()
        .filter_map(context_text)
        .any(|text| text.contains(needle))
}

fn shell_tool_spec(h: &Harness) -> ToolSpec {
    h.registry
        .providers_for("shell")
        .into_iter()
        .find(|provider| provider.tool.name == "shell")
        .expect("shell provider")
        .tool
}

fn unregister_shell(h: &mut Harness) {
    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    h.handle_extension_event(
        &conn_id,
        TestProtocolItem::Event(Event::ToolUnregister(tau_proto::ToolUnregister {
            tool_name: ToolName::new("shell"),
        })),
    )
    .expect("unregister shell");
}

fn reregister_shell(h: &mut Harness, spec: ToolSpec) {
    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    h.handle_extension_event(
        &conn_id,
        TestProtocolItem::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: spec,
            tool_group: None,
            prompt_fragment: None,
        })),
    )
    .expect("reregister shell");
}

fn staged_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: Some(format!("{name} test tool")),
        parameters: None,
        tool_type: tau_proto::ToolType::Function,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: Some(tau_proto::BackgroundSupport::Never),
        examples: Vec::new(),
    }
}

fn staged_provider_model(id: &str) -> tau_proto::ProviderModelInfo {
    tau_proto::ProviderModelInfo {
        id: id.into(),
        display_name: Some("Staged".to_owned()),
        tags: Vec::new(),
        supported_tool_types: vec![],
        default_affinity: 100,
        context_window: 4_096,
        efforts: vec![tau_proto::Effort::Medium],
        verbosities: vec![tau_proto::Verbosity::Medium],
        thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_threshold: None,
    }
}

fn clear_quiet_provider_models(h: &mut Harness) {
    let provider_id = h
        .extension_connection_id("provider")
        .expect("provider")
        .to_owned();
    h.handle_extension_event(
        &provider_id,
        TestProtocolItem::Event(Event::ProviderModelsUpdated(
            tau_proto::ProviderModelsUpdated { models: Vec::new() },
        )),
    )
    .expect("clear provider models");
}

fn connect_handshaking_extension(
    h: &mut Harness,
    conn_id: &str,
    kind: tau_proto::ClientKind,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let sink = connect_test_client(h, conn_id, kind.clone());
    let connection_id: tau_proto::ConnectionId = conn_id.into();
    h.extensions.entries.insert(
        connection_id.clone(),
        ExtensionEntry {
            name: conn_id.to_owned(),
            instance_id: 42.into(),
            connection_id: connection_id.clone(),
            kind,
            require: true,
            respawn_allowed: true,
            pid: None,
            in_process_thread: None,
            supervised_config: None,
            secrets: BTreeMap::new(),
            restart_attempt: 0,
            state: ExtensionState::Handshaking,
            protocol_io: tau_client::ProtocolIoMeter::default(),
        },
    );
    h.extensions.order.push(connection_id);
    sink
}

fn connect_handshaking_tool(h: &mut Harness, conn_id: &str) -> Arc<Mutex<Vec<RoutedFrame>>> {
    connect_handshaking_extension(h, conn_id, tau_proto::ClientKind::Tool)
}

fn insert_extension_entry_with_meter(
    h: &mut Harness,
    connection_id: &str,
    name: &str,
    state: ExtensionState,
    protocol_io: tau_client::ProtocolIoMeter,
) {
    let connection_id: tau_proto::ConnectionId = connection_id.into();
    h.extensions.entries.insert(
        connection_id.clone(),
        ExtensionEntry {
            name: name.to_owned(),
            instance_id: 42.into(),
            connection_id: connection_id.clone(),
            kind: tau_proto::ClientKind::Tool,
            require: true,
            respawn_allowed: true,
            pid: None,
            in_process_thread: None,
            supervised_config: None,
            secrets: BTreeMap::new(),
            restart_attempt: 0,
            state,
            protocol_io,
        },
    );
    h.extensions.order.push(connection_id);
}

fn first_notice(sink: &Arc<Mutex<Vec<RoutedFrame>>>) -> tau_proto::HarnessNotice {
    let frames = sink.lock().expect("sink");
    for routed in frames.iter() {
        if let Some(Event::HarnessNotice(notice)) = peel_inner_event(&routed.frame) {
            return notice.clone();
        }
    }
    panic!("missing harness notice in frames: {frames:?}");
}

fn connect_socket_ui(
    h: &mut Harness,
) -> (
    tau_proto::ConnectionId,
    HarnessOutputReader<BufReader<UnixStream>>,
) {
    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let client_id = h.accept_client(server_end).expect("accept client");
    (
        client_id,
        HarnessOutputReader::new(BufReader::new(client_end)),
    )
}

fn read_notice<R: std::io::Read>(reader: &mut HarnessOutputReader<R>) -> tau_proto::HarnessNotice {
    let message = reader
        .read_message()
        .expect("read notice")
        .expect("notice frame");
    let Some(Event::HarnessNotice(notice)) = peel_inner_event(&message) else {
        panic!("expected harness notice, got {message:?}");
    };
    notice.clone()
}

fn assert_no_message<R: std::io::Read>(reader: &mut HarnessOutputReader<R>) {
    match reader.read_message() {
        Err(tau_proto::DecodeError::Io(error)) if error.kind() == ErrorKind::WouldBlock => {}
        Ok(None) => {}
        other => panic!("unexpected routed frame: {other:?}"),
    }
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

    h.handle_client_event_inner(
        requesting_ui_id.as_str(),
        Event::UiDebugEventStatsRequest(tau_proto::UiDebugEventStatsRequest {
            extension_name: "std-shell".to_owned(),
        }),
    )
    .expect("request stats");

    let notice = read_notice(&mut requesting_ui);
    assert!(notice.always_show);
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

/// Extension input frames should be counted through the normal harness message
/// intake path before the debug command reads the live extension's meter.
#[test]
fn debug_event_stats_request_reports_recorded_extension_input() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    connect_handshaking_tool(&mut h, "std-shell");

    h.handle_extension_event(
        "std-shell",
        TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "std-shell".into(),
            client_kind: tau_proto::ClientKind::Tool,
        })),
    )
    .expect("extension hello");
    h.handle_client_event_inner(
        ui_id.as_str(),
        Event::UiDebugEventStatsRequest(tau_proto::UiDebugEventStatsRequest {
            extension_name: "std-shell".to_owned(),
        }),
    )
    .expect("request stats");

    let notice = read_notice(&mut ui);
    assert!(notice.always_show);
    assert!(notice.message.contains("message.hello:"));
    assert!(notice.message.contains("extension -> harness:"));
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

    h.handle_client_event_inner(
        "ui",
        Event::UiDebugEventStatsRequest(tau_proto::UiDebugEventStatsRequest {
            extension_name: "secret-ext".to_owned(),
        }),
    )
    .expect("request stats");

    let notice = first_notice(&ui);
    assert!(notice.always_show);
    assert!(
        notice
            .message
            .contains("only available to local socket clients")
    );
    assert!(!notice.message.contains("secret.extension_event"));
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

    h.handle_client_event_inner(
        ui_id.as_str(),
        Event::UiDebugEventStatsRequest(tau_proto::UiDebugEventStatsRequest {
            extension_name: "std-shell".to_owned(),
        }),
    )
    .expect("request stats");

    let notice = read_notice(&mut ui);
    assert!(notice.always_show);
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

    h.handle_client_event_inner(
        ui_id.as_str(),
        Event::UiDebugEventStatsRequest(tau_proto::UiDebugEventStatsRequest {
            extension_name: "std-shell".to_owned(),
        }),
    )
    .expect("request stats");

    let notice = read_notice(&mut ui);
    assert!(notice.always_show);
    assert!(
        notice
            .message
            .contains("extension name `std-shell` matched 2 live connections")
    );
}

fn sink_has_tool_invoke(sink: &Arc<Mutex<Vec<RoutedFrame>>>, call_id: &str) -> bool {
    sink.lock().expect("sink").iter().any(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::ToolStarted(invoke)) if invoke.call_id.as_str() == call_id
        )
    })
}

fn test_tool_result(call_id: &str, tool_name: &str) -> Event {
    Event::ToolResult(ToolResult {
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("ok".to_owned()),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,

        display: None,
    })
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

    h.handle_extension_event(
        "std-email",
        TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "tau-ext-pim".into(),
            client_kind: tau_proto::ClientKind::Tool,
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

    h.handle_extension_event(
        "std-email",
        TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "tau-ext-pim".into(),
            client_kind: tau_proto::ClientKind::Tool,
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

#[test]
fn extension_config_error_is_mandatory_warning_and_replayed_to_late_ui() {
    // Extension config validation often runs during daemon startup, before the
    // terminal UI has subscribed. This is regression coverage for the user
    // contract: any extension `ConfigError` must become a mandatory warning
    // `harness.notice` visible to late UI clients, not just a debug-log line.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "config-bad-ext";
    let _extension_sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_message(
        conn_id,
        TestMessage::ConfigError(tau_proto::ConfigError {
            message: "unknown field `enforce_ro_mode`".to_owned(),
        }),
    )
    .expect("config error handled");

    assert!(event_log_contains_source_event(
        &h,
        "harness",
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_CONFIG_ERROR
                    && info.always_show
                    && info.message.contains("extension config-bad-ext rejected its config")
                    && info.message.contains("unknown field `enforce_ro_mode`")
        )
    ));

    let ui_conn: tau_proto::ConnectionId = "late-ui".into();
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
                && info.always_show
                && info.message.contains("extension config-bad-ext rejected its config")
                && info.message.contains("unknown field `enforce_ro_mode`")
    )));
}

#[test]
fn extension_authored_notice_kind_is_stable_and_sanitized() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "ext-conn-unstable";
    let _extension_sink = connect_test_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::HarnessNotice(tau_proto::HarnessNotice {
            kind: "spoofed.kind".to_owned(),
            message: "extension authored".to_owned(),
            level: tau_proto::NoticeLevel::Critical,
            always_show: true,
        })),
    )
    .expect("extension notice handled");

    assert!(event_log_contains_source_event(
        &h,
        conn_id,
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.kind == tau_proto::notice_kind::EXTENSION_NOTICE
                    && info.message == "extension authored"
                    && info.level == tau_proto::NoticeLevel::Warning
                    && !info.always_show
        )
    ));
}

#[test]
fn optional_extension_config_error_is_replayed_and_disables_extension() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "optional-config-bad-ext";
    let _extension_sink = connect_handshaking_tool(&mut h, conn_id);
    h.extensions
        .entries
        .get_mut(conn_id)
        .expect("extension entry")
        .require = false;

    h.handle_extension_message(
        conn_id,
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
        "harness",
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
        "harness",
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                    && info.always_show
                    && info.message == "optional extension optional-config-bad-ext did not initialize"
        )
    ));

    let ui_conn: tau_proto::ConnectionId = "late-ui-optional-config".into();
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
                && info.always_show
                && info.message == "optional extension optional-config-bad-ext did not initialize"
    )));
}

#[test]
fn optional_extension_spawn_failure_is_mandatory_warning_and_nonfatal() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let config = crate::settings::Config {
        core: crate::settings::CoreConfig {
            mode: crate::settings::CoreMode::Embedded,
        },
        extensions: BTreeMap::from([(
            "optional-spawn-bad".to_owned(),
            crate::settings::ExtensionConfig {
                name: "optional-spawn-bad".to_owned(),
                command: "/definitely/not/a/tau-extension".to_owned(),
                args: Vec::new(),
                role: None,
                require: false,
                cwd: None,
                config: serde_json::json!({}),
                secrets: BTreeMap::new(),
            },
        )]),
        extension_startup_diagnostics: Vec::new(),
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

    assert!(h.extension_connection_id("optional-spawn-bad").is_none());
    assert!(event_log_contains_source_event(
        &h,
        "harness",
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                    && info.always_show
                    && info.message == "optional extension optional-spawn-bad did not initialize"
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

    h.handle_startup_disconnect(conn_id)
        .expect("optional pre-ready disconnect should not fail startup");

    let entry = h.extensions.entries.get(conn_id).expect("extension entry");
    assert_eq!(entry.state, ExtensionState::Disconnected);
    assert!(!entry.respawn_allowed);
    assert!(event_log_contains_source_event(
        &h,
        "harness",
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                    && info.always_show
                    && info.message == "optional extension optional-pre-ready-drop did not initialize"
        )
    ));

    let ui_conn: tau_proto::ConnectionId = "late-ui-pre-ready-drop".into();
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
                && info.always_show
                && info.message == "optional extension optional-pre-ready-drop did not initialize"
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
        "harness",
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                    && info.always_show
                    && info.message == "optional extension optional-timeout-ext did not initialize"
        )
    ));

    let ui_conn: tau_proto::ConnectionId = "late-ui-timeout".into();
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
                && info.always_show
                && info.message == "optional extension optional-timeout-ext did not initialize"
    )));
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
        "harness",
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.message.contains("startup timed out waiting for required extension")
                    && info.message.contains("required-timeout-ext")
        )
    ));
}

#[test]
fn post_ready_optional_tool_disconnect_keeps_existing_respawn_policy_flag() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "optional-ready-tool";
    let _sink = connect_handshaking_tool(&mut h, conn_id);
    h.handle_extension_message(
        conn_id,
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

    h.handle_disconnect(conn_id);

    let entry = h.extensions.entries.get(conn_id).expect("extension entry");
    assert_eq!(entry.state, ExtensionState::Disconnected);
    assert!(entry.respawn_allowed);
}

#[test]
fn startup_diagnostics_are_mandatory_warning_and_replayed() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");

    h.emit_extension_startup_diagnostics(&[crate::settings::ExtensionStartupDiagnostic {
        extension: "optional-diagnostic".to_owned(),
        message: "optional extension optional-diagnostic did not initialize".to_owned(),
    }]);

    assert!(event_log_contains_source_event(
        &h,
        "harness",
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                    && info.always_show
                    && info.message == "optional extension optional-diagnostic did not initialize"
        )
    ));

    let ui_conn: tau_proto::ConnectionId = "late-ui-startup-diagnostic".into();
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
                && info.always_show
                && info.message == "optional extension optional-diagnostic did not initialize"
    )));
}

#[test]
fn harness_failure_notice_is_mandatory_warning() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");

    h.emit_harness_failure("failed to dispatch queued prompt: boom");

    assert!(event_log_contains_source_event(
        &h,
        "harness",
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.kind == tau_proto::notice_kind::HARNESS_FAILURE
                    && info.level == tau_proto::NoticeLevel::Warning
                    && info.always_show
                    && info.message == "failed to dispatch queued prompt: boom"
        )
    ));
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
    h.selected_model = Some("test/model".into());
    let conn_id = "conn-staged-before-ready";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("staged_tool"),
            tool_group: None,
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "staged_tool.instructions",
                tau_proto::PromptPriority::new(10),
                "STAGED TOOL PROMPT",
            )),
        })),
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

    assert!(h.registry.providers_for("staged_tool").is_empty());
    assert!(
        !h.gather_tool_definitions_for_role(&h.selected_role)
            .iter()
            .any(|tool| tool.name.as_str() == "staged_tool")
    );
    let system_prompt = h.build_system_prompt_for_role(&h.selected_role);
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

#[test]
fn staged_tool_register_activates_on_ready_and_prompts_include_it() {
    // Ready is the activation boundary: the staged tool and its prompt fragment
    // become visible together before any queued prompts are advanced.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let conn_id = "conn-staged-ready";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("staged_tool"),
            tool_group: None,
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "staged_tool.instructions",
                tau_proto::PromptPriority::new(10),
                "STAGED TOOL PROMPT",
            )),
        })),
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

    h.handle_extension_message(
        conn_id,
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert_eq!(h.registry.providers_for("staged_tool").len(), 1);
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

#[test]
fn tool_prompt_fragment_heading_uses_model_visible_tool_name() {
    // Tool prompt fragments are grouped by the tool name the model can call, so
    // the automatic heading must use the same model-visible alias as the tool
    // definition instead of the provider's internal routing name.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let conn_id = "conn-staged-visible-tool";
    let _sink = connect_handshaking_tool(&mut h, conn_id);
    let mut spec = staged_tool_spec("internal_staged_tool");
    spec.model_visible_name = Some(ToolName::new("visible_staged_tool"));

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: spec,
            tool_group: None,
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "visible_staged_tool.instructions",
                tau_proto::PromptPriority::new(10),
                "ALIASED TOOL PROMPT",
            )),
        })),
    )
    .expect("stage tool");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("empty_fragment_tool"),
            tool_group: None,
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "empty_fragment_tool.instructions",
                tau_proto::PromptPriority::new(10),
                "",
            )),
        })),
    )
    .expect("stage empty prompt tool");
    h.handle_extension_message(
        conn_id,
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

#[test]
fn queued_tool_call_waits_for_staged_provider_until_ready() {
    // Regression: prompt-owned calls must use the prompt's advertised tool
    // snapshot, not current role policy. A tool can sit behind another
    // in-flight call after the live provider disappears, current policy changes
    // to disallow the tool, and a replacement provider is still handshaking.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let blocking_sink = connect_test_tool(&mut h, "conn-blocking-tool");
    h.registry
        .register("conn-blocking-tool", staged_tool_spec("blocking_tool"));
    let old_provider = connect_test_tool(&mut h, "conn-old-staged-tool");
    h.registry
        .register("conn-old-staged-tool", staged_tool_spec("staged_tool"));

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-staged-tools");
    h.prompt_agents
        .insert("sp-staged-tools".into(), cid.clone());
    assert!(
        h.prompt_tool_specs[&AgentPromptId::from("sp-staged-tools")]
            .iter()
            .any(|spec| spec.name == "staged_tool")
    );

    h.registry.unregister_connection("conn-old-staged-tool");
    drop(old_provider);
    h.available_roles
        .get_mut(&h.selected_role)
        .expect("selected role")
        .disable_tools
        .push(ToolName::new("staged_tool"));

    let staged_sink = connect_handshaking_tool(&mut h, "conn-staged-tool");
    h.handle_extension_event(
        "conn-staged-tool",
        TestProtocolItem::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("staged_tool"),
            tool_group: None,
            prompt_fragment: None,
        })),
    )
    .expect("stage tool");
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "run two tools".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-staged-tools".into(),
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
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    assert!(sink_has_tool_invoke(&blocking_sink, "call-blocking"));
    assert!(!sink_has_tool_invoke(&staged_sink, "call-staged"));
    assert_eq!(h.tool_turn.pending_len(), 1);

    h.handle_extension_event(
        "conn-blocking-tool",
        TestProtocolItem::Event(test_tool_result("call-blocking", "blocking_tool")),
    )
    .expect("blocking result");

    assert!(!sink_has_tool_invoke(&staged_sink, "call-staged"));
    assert_eq!(h.tool_turn.pending_len(), 1);
    assert_eq!(h.tool_turn.in_flight_len(), 0);

    h.handle_extension_message(
        "conn-staged-tool",
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(sink_has_tool_invoke(&staged_sink, "call-staged"));
    assert_eq!(
        h.pending_tool_providers
            .get("call-staged")
            .map(|provider| provider.as_str()),
        Some("conn-staged-tool")
    );

    h.handle_extension_event(
        "conn-staged-tool",
        TestProtocolItem::Event(test_tool_result("call-staged", "staged_tool")),
    )
    .expect("staged result");
    assert!(!h.pending_tool_providers.contains_key("call-staged"));

    h.shutdown().expect("shutdown");
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
    h.selected_model = Some("test/model".into());

    let staged_sink = connect_handshaking_tool(&mut h, "conn-unadvertised-staged-tool");
    h.handle_extension_event(
        "conn-unadvertised-staged-tool",
        TestProtocolItem::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("unadvertised_staged_tool"),
            tool_group: None,
            prompt_fragment: None,
        })),
    )
    .expect("stage tool");

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-unadvertised-staged");
    h.prompt_agents
        .insert("sp-unadvertised-staged".into(), cid.clone());
    assert!(
        !h.prompt_tool_specs[&AgentPromptId::from("sp-unadvertised-staged")]
            .iter()
            .any(|spec| spec.name == "unadvertised_staged_tool")
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-unadvertised-staged".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
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
    assert_eq!(h.tool_turn.pending_len(), 0);
    assert!(!sink_has_tool_invoke(&staged_sink, "call-unadvertised"));

    h.handle_extension_message(
        "conn-unadvertised-staged-tool",
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");
    assert!(!sink_has_tool_invoke(&staged_sink, "call-unadvertised"));

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
    h.selected_model = Some("test/model".into());
    let conn_id = "conn-never-ready";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("never_ready_tool"),
            tool_group: None,
            prompt_fragment: None,
        })),
    )
    .expect("stage tool");

    let submission = h
        .submit_user_prompt("s1".into(), "try never ready tool".to_owned())
        .expect("submit");
    assert!(matches!(submission, PromptSubmission::Queued));
    assert!(h.registry.providers_for("never_ready_tool").is_empty());
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
    assert!(h.selected_model.is_none());

    let conn_id = "conn-staged-provider";
    let _sink = connect_handshaking_extension(&mut h, conn_id, tau_proto::ClientKind::Provider);
    let model_name = "staged/provider-model";
    let model_id: tau_proto::ModelId = model_name.into();
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ProviderModelsUpdated(
            tau_proto::ProviderModelsUpdated {
                models: vec![staged_provider_model(model_name)],
            },
        )),
    )
    .expect("stage provider models");

    let submission = h
        .submit_user_prompt("s1".into(), "wait for staged model".to_owned())
        .expect("submit");
    assert!(matches!(submission, PromptSubmission::Queued));
    assert!(!h.available_models.contains(&model_id));
    assert!(!h.provider_model_routes.contains_key(&model_id));
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );
    assert!(!event_log_contains_source_event(&h, conn_id, |event| {
        matches!(event, Event::ProviderModelsUpdated(_))
    }));

    h.handle_extension_message(
        conn_id,
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(h.available_models.contains(&model_id));
    assert_eq!(
        h.provider_model_routes.get(&model_id).map(|id| id.as_str()),
        Some(conn_id)
    );
    assert!(event_log_contains_source_event(&h, conn_id, |event| {
        matches!(event, Event::ProviderModelsUpdated(update) if update.models.iter().any(|model| model.id == model_id))
    }));
    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(prompt.model, model_id);
    assert!(prompt_context_contains(&prompt, "wait for staged model"));

    h.shutdown().expect("shutdown");
}

#[test]
fn skill_agent_context_and_fragment_are_staged_until_ready() {
    // Skills, agent context, and extension prompt fragments all feed prompt
    // assembly. None of them may affect the system prompt until Ready activates
    // the staged batch.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let conn_id = "conn-staged-context";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
            name: "staged-skill".into(),
            description: "STAGED SKILL DESCRIPTION".to_owned(),
            file_path: "/tmp/staged-skill/SKILL.md".into(),
            add_to_prompt: true,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
        })),
    )
    .expect("stage skill");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agents[&cid]
        .agent_id
        .as_deref()
        .expect("agent id")
        .to_owned();
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtAgentContextPublish(
            tau_proto::ExtAgentContextPublish {
                agent_id: crate::parse_agent_id(&agent_id),
                key: "demo".into(),
                value: tau_proto::AgentContextValue(serde_json::json!({
                    "answer": "STAGED CONTEXT VALUE"
                })),
            },
        )),
    )
    .expect("stage agent context");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtPromptFragmentPublish(
            tau_proto::ExtPromptFragmentPublish {
                fragment: tau_proto::PromptFragment::new(
                    "staged.context.fragment",
                    tau_proto::PromptPriority::new(20),
                    "CTX={{#each agent_context.demo}}{{value.answer}}{{/each}}",
                ),
            },
        )),
    )
    .expect("stage prompt fragment");

    assert!(!h.discovered_skills.contains_key("staged-skill"));
    let prompt_agent_id = tau_proto::AgentId::parse(&agent_id).expect("agent id");
    let before_prompt = h
        .try_build_system_prompt_for_role_and_agent(&h.selected_role, Some(&prompt_agent_id), &[])
        .expect("prompt renders");
    assert!(!before_prompt.contains("STAGED SKILL DESCRIPTION"));
    assert!(!before_prompt.contains("STAGED CONTEXT VALUE"));

    h.handle_extension_message(
        conn_id,
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(h.discovered_skills.contains_key("staged-skill"));
    let after_prompt = h
        .try_build_system_prompt_for_role_and_agent(&h.selected_role, Some(&prompt_agent_id), &[])
        .expect("prompt renders");
    assert!(after_prompt.contains("STAGED SKILL DESCRIPTION"));
    assert!(after_prompt.contains("STAGED CONTEXT VALUE"));
    assert!(
        !event_log_events(&h).iter().any(|event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.message.contains("extension.agent_context_publish rejected")
        )),
        "agent context publishes update prompt context but must not be persisted as agent transcript events"
    );

    h.shutdown().expect("shutdown");
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
    h.initialized_sessions
        .remove(&tau_proto::SessionId::new("s1"));

    h.handle_extension_message(
        "early-session-dir",
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
    h.initialized_sessions
        .remove(&tau_proto::SessionId::new("s1"));

    h.handle_client_message(
        "startup-ui",
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

    h.replay_harness_notice("startup-ui", &selectors);
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

#[test]
fn agents_context_ready_staged_until_ready_and_queue_waits() {
    // AGENTS.md discovery and the matching context-ready acknowledgement are
    // startup context state. A queued user prompt must wait for Ready, then see
    // the injected AGENTS.md context in the dispatched prompt.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "conn-staged-agents";
    let _sink = connect_handshaking_tool(&mut h, conn_id);
    h.initialized_sessions.remove("s1");
    h.turn_state = TurnState::InitializingSession {
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: [tau_proto::ConnectionId::from(conn_id)]
            .into_iter()
            .collect(),
    };

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtAgentsMdAvailable(
            tau_proto::ExtAgentsMdAvailable {
                file_path: "/repo/AGENTS.md".into(),
                content: "# Rules\nSTAGED AGENTS CONTEXT".to_owned(),
            },
        )),
    )
    .expect("stage agents");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                session_id: "s1".into(),
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            },
        )),
    )
    .expect("stage context ready");
    let submission = h
        .submit_user_prompt("s1".into(), "queued after staged context".to_owned())
        .expect("submit");

    assert!(matches!(submission, PromptSubmission::Queued));
    assert!(h.discovered_agents_files.is_empty());
    assert!(matches!(
        h.turn_state,
        TurnState::InitializingSession { .. }
    ));
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );
    assert!(!event_log_contains_source_event(&h, conn_id, |event| {
        matches!(
            event,
            Event::ExtAgentsMdAvailable(_) | Event::ExtensionContextReady(_)
        )
    }));

    h.handle_extension_message(
        conn_id,
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(h.initialized_sessions.contains("s1"));
    assert!(event_log_contains_source_event(&h, conn_id, |event| {
        matches!(
            event,
            Event::ExtAgentsMdAvailable(_) | Event::ExtensionContextReady(_)
        )
    }));
    let prompt = read_nth_prompt_created(&h, 0);
    assert!(prompt_context_contains(
        &prompt,
        "queued after staged context"
    ));
    assert!(prompt_context_contains(&prompt, "STAGED AGENTS CONTEXT"));

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
        conn_id,
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
    h.bus
        .set_subscriptions(
            "session-context-ready-observer",
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
                session_id: "s1".into(),
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
        conn_id,
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
        conn_id,
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");
    h.publish_event(None, draft_event("after ready"));

    assert!(sink.lock().expect("sink").iter().any(|routed| {
        matches!(&routed.frame, HarnessOutputMessage::InterceptRequest(req)
            if matches!(req.event.as_ref(), Event::UiPromptDraft(draft) if draft.text == "after ready"))
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn extension_emit_and_start_agent_request_are_staged_until_ready() {
    // Generic emits are visible bus state, and StartAgentRequest starts prompt
    // dispatch. Both are held until Ready so a handshaking extension cannot
    // publish or start side-agent work early.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "conn-staged-emit-query";
    let _sink = connect_handshaking_tool(&mut h, conn_id);
    let custom_name: tau_proto::EventName = "demo.startup_state".parse().expect("event name");

    h.handle_extension_message(
        conn_id,
        TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ExtensionEvent(
                tau_proto::CustomEvent::try_new(
                    custom_name.clone(),
                    Some("s1".into()),
                    CborValue::Text("STAGED CUSTOM EVENT".to_owned()),
                )
                .expect("valid custom event"),
            )),
            transient: false,
        }),
    )
    .expect("stage emit");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::StartAgentRequest(StartAgentRequest {
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

    assert!(!event_log_contains_source_event(&h, conn_id, |event| {
        event.name() == custom_name
    }));
    assert!(!h.agents.keys().any(|cid| cid.as_str().contains("q-staged")));
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );

    h.handle_extension_message(
        conn_id,
        TestMessage::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(event_log_contains_source_event(&h, conn_id, |event| {
        event.name() == custom_name
    }));
    assert!(h.agents.iter().any(|(cid, conv)| {
        conv.agent_id.as_deref() == Some(cid.as_str())
            && matches!(
                &conv.originator,
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
    h.selected_model = Some("test/model".into());
    let conn_id = "conn-agent-context-ready";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_message(
        conn_id,
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
        conn_id,
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

    h.dispatch_user_prompt("s1".into(), "first prompt".to_owned())
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
        .agents
        .values()
        .find(|agent| agent.originator.is_user())
        .and_then(|agent| agent.agent_id.as_deref())
        .expect("durable user agent")
        .to_owned();
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtAgentContextPublish(
            tau_proto::ExtAgentContextPublish {
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
                session_id: "s1".into(),
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
fn disconnect_before_ready_drops_all_staged_state() {
    // If a handshaking extension goes away, its staged batch is discarded rather
    // than becoming visible through model routes, prompt assembly, interceptors,
    // custom events, or tool routing.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    clear_quiet_provider_models(&mut h);
    let conn_id = "conn-drop-staged";
    let sink = connect_handshaking_extension(&mut h, conn_id, tau_proto::ClientKind::Provider);
    let model_name = "staged/drop-model";
    let model_id: tau_proto::ModelId = model_name.into();

    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("dropped_tool"),
            tool_group: None,
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "dropped.tool.fragment",
                tau_proto::PromptPriority::new(10),
                "DROPPED TOOL FRAGMENT",
            )),
        })),
    )
    .expect("stage tool");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ProviderModelsUpdated(
            tau_proto::ProviderModelsUpdated {
                models: vec![staged_provider_model(model_name)],
            },
        )),
    )
    .expect("stage models");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
            name: "dropped-skill".into(),
            description: "DROPPED SKILL".to_owned(),
            file_path: "/tmp/dropped/SKILL.md".into(),
            add_to_prompt: true,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
        })),
    )
    .expect("stage skill");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtAgentsMdAvailable(
            tau_proto::ExtAgentsMdAvailable {
                file_path: "/repo/DROPPED.md".into(),
                content: "DROPPED AGENTS".to_owned(),
            },
        )),
    )
    .expect("stage agents");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agents[&cid]
        .agent_id
        .as_deref()
        .expect("agent id")
        .to_owned();
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtAgentContextPublish(
            tau_proto::ExtAgentContextPublish {
                agent_id: crate::parse_agent_id(&agent_id),
                key: "dropped".into(),
                value: tau_proto::AgentContextValue(serde_json::json!("DROPPED CONTEXT")),
            },
        )),
    )
    .expect("stage agent context");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtPromptFragmentPublish(
            tau_proto::ExtPromptFragmentPublish {
                fragment: tau_proto::PromptFragment::new(
                    "dropped.extension.fragment",
                    tau_proto::PromptPriority::new(20),
                    "DROPPED EXTENSION FRAGMENT",
                ),
            },
        )),
    )
    .expect("stage fragment");
    h.handle_extension_message(
        conn_id,
        TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        }),
    )
    .expect("stage intercept");
    h.handle_extension_message(
        conn_id,
        TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ExtensionEvent(
                tau_proto::CustomEvent::try_new(
                    "demo.dropped".parse().expect("event name"),
                    Some("s1".into()),
                    CborValue::Text("DROPPED EVENT".to_owned()),
                )
                .expect("valid custom event"),
            )),
            transient: false,
        }),
    )
    .expect("stage emit");

    h.handle_disconnect(conn_id);
    h.publish_event(None, draft_event("after disconnect"));

    assert!(!h.extensions.activation_staging.contains_key(conn_id));
    assert!(h.registry.providers_for("dropped_tool").is_empty());
    assert!(!h.available_models.contains(&model_id));
    assert!(!h.provider_model_routes.contains_key(&model_id));
    assert!(!h.discovered_skills.contains_key("dropped-skill"));
    assert!(h.discovered_agents_files.is_empty());
    assert!(
        !h.agent_context
            .template_value(Some(&crate::parse_agent_id(&agent_id)))
            .to_string()
            .contains("DROPPED CONTEXT")
    );
    assert!(
        !h.build_system_prompt_for_role(&h.selected_role)
            .contains("DROPPED")
    );
    assert!(!event_log_contains_source_event(&h, conn_id, |event| {
        event.name().to_string().contains("dropped")
    }));
    assert!(
        sink.lock()
            .expect("sink")
            .iter()
            .all(|routed| { !matches!(routed.frame, HarnessOutputMessage::InterceptRequest(_)) })
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn tool_unregister_removes_tool_from_future_prompt() {
    // Regression: an explicit ToolUnregister must update the live registry used
    // for future prompt assembly while leaving old prompt snapshots intact.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "before unregister");
    let before_spid = h.send_prompt_to_agent("s1");
    let before_prompt = read_prompt_created(&h, &before_spid);
    assert!(prompt_has_tool(&before_prompt, "shell"));

    unregister_shell(&mut h);

    append_user_message_via_event(&mut h, "s1", "after unregister");
    let after_spid = h.send_prompt_to_agent("s1");
    let after_prompt = read_prompt_created(&h, &after_spid);

    assert!(prompt_has_tool(&before_prompt, "shell"));
    assert!(!prompt_has_tool(&after_prompt, "shell"));

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
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "use shell");
    let spid = h.send_prompt_to_agent("s1");
    let old_prompt = read_prompt_created(&h, &spid);
    assert!(prompt_has_tool(&old_prompt, "shell"));

    unregister_shell(&mut h);

    h.handle_provider_response_finished(ProviderResponseFinished {
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
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
    h.selected_model = Some("test/model".into());

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
        .position(|item| context_text(item) == Some(notice.as_str()))
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
    h.selected_model = Some("test/model".into());

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
    h.selected_model = Some("test/model".into());

    let spec = shell_tool_spec(&h);
    let unavailable = tool_unavailable_notice_prompt(&ToolName::new("shell"));
    let available = tool_available_again_notice_prompt(&ToolName::new("shell"));
    unregister_shell(&mut h);

    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("after unregister".to_owned()))
        .expect("dispatch unavailable prompt");
    let first_prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(context_text_count(&first_prompt, &unavailable), 1);
    h.handle_provider_response_finished(super::dispatch::provider_text_response(
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
        .position(|item| context_text(item) == Some(available.as_str()))
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

#[test]
fn duplicate_provider_keeps_tool_available_without_notice() {
    // Removing one provider must not hide the tool if another provider for the
    // same tool name remains registered.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let spec = shell_tool_spec(&h);
    h.registry.register("conn-duplicate-shell", spec);
    let notice = tool_unavailable_notice_prompt(&ToolName::new("shell"));

    unregister_shell(&mut h);
    assert_eq!(h.registry.providers_for("shell").len(), 1);

    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::user("after partial unregister".to_owned()),
    )
    .expect("dispatch user prompt");

    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(context_text_count(&prompt, &notice), 0);
    assert_eq!(agent_prompt_text_count(&h, &notice), 0);
    assert!(prompt_has_tool(&prompt, "shell"));

    h.shutdown().expect("shutdown");
}

#[test]
fn unavailable_tool_is_reported_without_crashing() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let removed = h.registry.unregister_connection(&conn_id);
    assert!(removed.iter().any(|t| t == "shell"));

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_agents.insert("sp-x".into(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "shell printf hi".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    let target_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-x".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
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
            agent_prompt_id: "sp-main".into(),
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
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    h.tool_agents.insert(call_id.clone(), cid.clone());
    h.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: tool_name.clone(),
            internal_name: tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.pending_tool_providers
        .insert(call_id.clone(), conn_id.clone().into());
    h.tool_turn
        .record_in_flight_for_test(cid.clone(), call_id.clone());
    if let Some(conv) = h.agents.get_mut(&cid) {
        conv.turn_state = AgentTurnState::ToolsRunning {
            remaining_calls: vec![call_id.clone()],
        };
    }

    h.handle_disconnect(&conn_id);

    // Disconnect publishes a ToolError, drops the call from the
    // conversation's `ToolsRunning` set, and — since that was the
    // last outstanding call — re-prompts the agent so it can react
    // to the failure. The conversation therefore transitions
    // `ToolsRunning -> AgentThinking`, not back to `Idle`.
    assert!(matches!(h.turn_state, TurnState::Idle));
    assert!(matches!(
        h.agents
            .get(&test_user_agent(&h))
            .expect("default conversation")
            .turn_state,
        AgentTurnState::AgentThinking { .. }
    ));
    assert!(!h.tool_agents.contains_key(&call_id));
    assert!(!h.pending_tool_providers.contains_key(&call_id));

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
    h.selected_model = Some("test/model".into());

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Send disconnect to the extension via the bus (through the
    // writer channel → writer thread → stream).
    let _ = h.bus.send_to(
        &conn_id,
        None,
        HarnessOutputMessage::Disconnect(Disconnect {
            reason: Some("test".to_owned()),
        }),
    );

    // Drive event loop until the disconnect arrives.
    let started = Instant::now();
    loop {
        let event =
            h.rx.recv_timeout(Duration::from_secs(2))
                .expect("should get disconnect");
        match event {
            HarnessEvent::Disconnected {
                ref connection_id, ..
            } if *connection_id == conn_id => {
                h.handle_disconnect(&conn_id);
                break;
            }
            HarnessEvent::FromConnection {
                connection_id,
                message,
            } => {
                let _ = h.handle_extension_message(&connection_id, *message);
            }
            _ => {}
        }
        assert!(started.elapsed() < Duration::from_secs(2), "timeout");
    }

    assert!(h.bus.connection(&conn_id).is_none());
    assert!(h.registry.providers_for("shell").is_empty());
    assert!(
        h.lifecycle_messages
            .iter()
            .any(|m| m == "extension shell exited")
    );

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_agents.insert("sp-x".into(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "shell printf hi".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-x".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
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
                    client_name: "late-tool".into(),
                    client_kind: tau_proto::ClientKind::Tool,
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
        &h.tx,
    )
    .expect("spawn late tool");
    let conn_id = spawned.connection_id.clone();
    h.queue_extension_connect(ExtensionConnectCommand {
        entry: ExtensionEntry {
            name: "late-tool".to_owned(),
            instance_id: 999.into(),
            connection_id: conn_id.clone(),
            kind: tau_proto::ClientKind::Tool,
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
        replaces: None,
    })
    .expect("queue connect command");

    assert!(h.bus.connection(&conn_id).is_none());
    assert!(!h.extensions.entries.contains_key(&conn_id));

    let event =
        h.rx.recv_timeout(Duration::from_secs(1))
            .expect("connect command should be first");
    match event {
        HarnessEvent::Command(command) => h.handle_harness_command(command).expect("handle"),
        HarnessEvent::FromConnection { .. }
        | HarnessEvent::Disconnected { .. }
        | HarnessEvent::NewClient(_) => panic!("reader forwarded before connect command"),
    }

    assert!(h.bus.connection(&conn_id).is_some());
    assert!(h.extensions.entries.contains_key(&conn_id));
    assert!(
        h.lifecycle_messages
            .iter()
            .any(|m| m == "extension late-tool starting")
    );

    let event =
        h.rx.recv_timeout(Duration::from_secs(1))
            .expect("reader should forward after connect ack");
    match event {
        HarnessEvent::FromConnection {
            connection_id,
            message,
        } => {
            assert_eq!(connection_id, conn_id);
            assert!(matches!(message.as_ref(), HarnessInputMessage::Hello(_)));
        }
        HarnessEvent::Command(_)
        | HarnessEvent::Disconnected { .. }
        | HarnessEvent::NewClient(_) => panic!("unexpected harness event after connect ack"),
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

    h.selected_model = Some("test/model".into());
    h.selected_role = "engineer".to_owned();
    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_agents.insert("sp-x".into(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "do it".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-x".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
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

/// Ensures a failed direct provider prompt route unwinds in-flight prompt
/// bookkeeping and emits user-visible lifecycle diagnostics.
#[test]
fn provider_prompt_route_failure_clears_prompt_bookkeeping() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let model: tau_proto::ModelId = "test/model".into();
    h.provider_model_routes
        .insert(model.clone(), "missing-provider".into());
    h.agents.get_mut(&cid).expect("agent").model_override = Some(model);

    let agent_prompt_id = h
        .send_prompt_to_agent_for(&cid)
        .expect("prompt should be constructed before route failure");

    assert!(!h.prompt_agents.contains_key(agent_prompt_id.as_str()));
    assert!(!h.prompt_models.contains_key(&agent_prompt_id));
    assert!(!h.pending_provider_prompts.contains_key(&agent_prompt_id));
    let conv = h.agents.get(&cid).expect("agent still loaded");
    assert_eq!(conv.in_flight_prompt, None);
    assert_eq!(conv.last_prompt_id, None);
    assert!(matches!(conv.turn_state, AgentTurnState::Idle));
    assert_eq!(h.current_session_state.token_usage.total.requests, 0);

    let events = event_log_events(&h);
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentPromptTerminated(terminated)
            if terminated.agent_prompt_id == agent_prompt_id
                && terminated.reason == tau_proto::AgentPromptTerminationReason::Canceled
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::HarnessNotice(info)
            if info.message.contains("provider prompt route failed")
                && info.message.contains(agent_prompt_id.as_str())
    )));

    h.shutdown().expect("shutdown");
}

/// Ensures targetless user shell output is routed to the default user agent
/// instead of panicking when the shell extension omits a target agent id.
#[test]
fn targetless_shell_output_injects_into_default_agent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.inject_user_shell_output(&tau_proto::ShellCommandFinished {
        command_id: "shell-1".into(),
        session_id: "s1".into(),
        command: "printf hello".to_owned(),
        include_in_context: true,
        target_agent_id: None,
        output: "hello".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    });

    let injected = loaded_agent_events(&h, "s1")
        .into_iter()
        .find_map(|event| match event {
            Event::AgentUserMessageInjected(injected) if injected.text.contains("printf hello") => {
                Some(injected)
            }
            _ => None,
        })
        .expect("shell output injected into agent transcript");
    assert_eq!(injected.agent_id, agent_id);
    assert!(injected.text.contains("<user_shell"));
    assert!(injected.text.contains("hello"));
}

/// Late shell completion must not append new durable work after terminal
/// teardown has begun, whether explicitly or implicitly targeted.
#[test]
fn terminating_agent_rejects_late_shell_output() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.agents.get_mut(&cid).expect("agent").terminating = true;

    for target_agent_id in [Some(agent_id.clone()), None] {
        h.inject_user_shell_output(&tau_proto::ShellCommandFinished {
            command_id: "late-shell".into(),
            session_id: "s1".into(),
            command: "printf late".to_owned(),
            include_in_context: true,
            target_agent_id,
            output: "late".to_owned(),
            exit_code: Some(0),
            cancelled: false,
        });
    }
    assert!(
        !loaded_agent_events(&h, "s1")
            .into_iter()
            .any(|event| matches!(
                event,
                Event::AgentUserMessageInjected(injected) if injected.text.contains("printf late")
            ))
    );
}

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
        command_id: "shell-2".into(),
        session_id: "other-session".into(),
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
fn agents_context_is_injected_when_agent_is_created() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_connection_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Eager init at construction may have already appended a real
    // AGENTS.md (ext-shell walks the test cwd). Clear so we assert
    // only on the test-injected pair below.
    h.discovered_agents_files.clear();
    h.discovered_agents_files.push(DiscoveredAgentsFile {
        source_id: tools_connection_id.clone().into(),
        file_path: PathBuf::from("/repo/AGENTS.md"),
        content: "# Root\n- root rule\n".to_owned(),
    });
    h.discovered_agents_files.push(DiscoveredAgentsFile {
        source_id: tools_connection_id.clone().into(),
        file_path: PathBuf::from("/repo/pkg/AGENTS.md"),
        content: "# Package\n- package rule\n".to_owned(),
    });
    let _cid = ensure_test_user_agent(&mut h);

    let events = loaded_agent_events(&h, "s1");
    let injected = events
        .iter()
        .rev()
        .find_map(|event| match event {
            Event::AgentUserMessageInjected(injected)
                if injected.text.contains("# AGENTS.md instructions")
                    && injected.text.contains("/repo/pkg") =>
            {
                Some(injected.text.as_str())
            }
            _ => None,
        })
        .expect("expected injected AGENTS.md user message");
    assert!(injected.contains("# AGENTS.md instructions"));
    assert!(injected.contains("<AGENTS_FILE path=\"/repo/pkg/AGENTS.md\">"));
    assert!(injected.contains("<AGENTS_FILE path=\"/repo/AGENTS.md\">"));
    assert!(injected.contains("</AGENTS_FILE>"));
    let root_pos = injected.find("root rule").expect("root rule");
    let pkg_pos = injected.find("package rule").expect("package rule");
    assert!(
        root_pos < pkg_pos,
        "broader file should appear before nested one"
    );

    h.shutdown().expect("shutdown");
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

    h.discovered_agents_files.clear();
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

    h.discovered_agents_files.push(DiscoveredAgentsFile {
        source_id: tools_connection_id.clone().into(),
        file_path: PathBuf::from("/repo/AGENTS.md"),
        content: format!("# Root\n- {marker}\n"),
    });
    h.pending_notices.restore_sessions.insert("s1".into(), None);
    h.turn_state = TurnState::InitializingSession {
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Resume,
        waiting_on: [tools_connection_id.clone().into()].into_iter().collect(),
    };
    h.handle_extension_event(
        &tools_connection_id,
        TestProtocolItem::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                session_id: "s1".into(),
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            },
        )),
    )
    .expect("ready");

    assert!(matches!(h.turn_state, TurnState::Idle));
    assert_eq!(count_marker_injections(&h), 1);
    assert!(
        h.pending_notices.restore_sessions.contains_key("s1"),
        "restore notice queue should be independent from AGENTS.md injection"
    );

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
    h.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_agents.insert("sp-x".into(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "do it".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    let response = ProviderResponseFinished {
        agent_prompt_id: "sp-x".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };

    h.handle_provider_response_finished(response)
        .expect("invalid tool call must not panic");

    // The call must be gone from both the pending queue and the
    // in-flight set — rejection fully completes it.
    assert!(h.tool_turn.is_empty());

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

    h.selected_model = Some("test/model".into());
    h.registry.register(
        "conn-delegate",
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
    h.prompt_agents.insert("sp-x".into(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "do it".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-x".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("empty call ids should be terminalized as tool errors");

    assert!(h.tool_turn.is_empty());
    assert!(!h.pending_tools.contains_key(&ToolCallId::from("")));
    assert!(!h.tool_agents.contains_key(&ToolCallId::from("")));

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
    h.prompt_agents.insert("sp-x".into(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-x".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("duplicate call ids should not wedge the harness");

    assert!(h.tool_turn.is_empty());
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
    h.prompt_agents.insert("sp-y".into(), cid.clone());
    h.completed_tool_calls.insert("old-call".into());

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-y".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("reused prior call id should not wedge the harness");

    assert!(h.tool_turn.is_empty());
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
    h.selected_model = Some("test/model".into());

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_agents.insert("sp-x".into(), cid.clone());
    let target_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_client_event(
        "ui",
        TestProtocolItem::Event(Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: "s1".into(),
            target_agent_id: Some(target_agent_id.clone()),
            agent_prompt_id: None,
        })),
    )
    .expect("cancel");

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-x".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("response");

    assert!(h.tool_turn.is_empty());
    assert!(matches!(
        h.agents.get(&cid).expect("conversation").turn_state,
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
    h.selected_model = Some("test/model".into());
    let _tool_events = connect_test_tool(&mut h, "conn-cancel-tools");
    h.registry
        .register("conn-cancel-tools", staged_tool_spec("slow_a"));
    h.registry
        .register("conn-cancel-tools", staged_tool_spec("slow_b"));

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_agents.insert("sp-x".into(), cid.clone());
    let target_agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-x".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("response");
    assert!(h.tool_turn.is_in_flight(&ToolCallId::from("c1")));
    assert!(h.tool_turn.is_in_flight(&ToolCallId::from("c2")));
    assert_eq!(h.tool_turn.pending_len(), 0);

    h.handle_client_event(
        "ui",
        TestProtocolItem::Event(Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: "s1".into(),
            target_agent_id: Some(target_agent_id),
            agent_prompt_id: None,
        })),
    )
    .expect("cancel");

    assert!(h.tool_turn.is_empty());
    assert!(matches!(
        h.agents.get(&cid).expect("conversation").turn_state,
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

    h.tx.send(HarnessEvent::Disconnected {
        connection_id: provider_id.into(),
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

#[test]
fn duplicate_tool_result_is_discarded() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let mut h = echo_harness(&sp).expect("start");

    // Fabricate a tool result for a call_id with no pending runtime metadata.
    let result = h.handle_extension_event(
        "fake-ext",
        TestProtocolItem::Event(Event::ToolResult(ToolResult {
            call_id: "orphan-call".into(),
            tool_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: tau_proto::CborValue::Text("stale data".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        })),
    );
    // Should not error — just emits a warning and discards.
    assert!(result.is_ok());
}

#[test]
fn hello_protocol_version_mismatch_is_rejected() {
    let hello = tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION + 1,
        client_name: "future-client".into(),
        client_kind: tau_proto::ClientKind::Tool,
    };

    let error = validate_protocol_version(&hello).expect_err("reject mismatched protocol");
    assert!(
        error
            .to_string()
            .contains("unsupported protocol version from future-client"),
        "unexpected error: {error}"
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
        .bus
        .connections()
        .into_iter()
        .find(|metadata| metadata.origin == ConnectionOrigin::Socket)
        .map(|metadata| metadata.id)
        .expect("socket client connection");
    assert!(h.bus.connection(socket_conn.as_str()).is_some());
    assert!(h.client_writers.contains_key(&socket_conn));

    h.tx.send(HarnessEvent::FromConnection {
        connection_id: socket_conn.clone(),
        message: Box::new(HarnessInputMessage::Disconnect(Disconnect {
            reason: Some("test explicit disconnect".to_owned()),
        })),
    })
    .expect("queue explicit disconnect");

    h.run_event_loop(Some(1), false).expect("event loop exits");

    assert!(h.bus.connection(socket_conn.as_str()).is_none());
    assert!(!h.client_writers.contains_key(&socket_conn));
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

    let error = std::io::Error::other("post-accept startup failure");
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

#[test]
fn client_hello_protocol_mismatch_disconnects_only_client() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let events = connect_test_client(&mut h, "stale-ui", tau_proto::ClientKind::Ui);

    let keep = h
        .handle_client_event(
            "stale-ui",
            TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION + 1,
                client_name: "stale-ui".into(),
                client_kind: tau_proto::ClientKind::Ui,
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
}

#[test]
fn extension_tool_request_cannot_reuse_in_flight_agent_call_id() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let owner_agent_id = durable_agent_id_for_conversation(&h, &cid);
    let call_id: ToolCallId = "shared-call".into();
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            agent_prompt_id: "sp-open".into(),
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
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    h.tool_agents.insert(call_id.clone(), cid.clone());
    h.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("read"),
            internal_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.pending_tool_providers
        .insert(call_id.clone(), "owner-ext".into());

    h.handle_extension_event(
        "hijacker-ext",
        TestProtocolItem::Event(Event::ToolRequest(tau_proto::ToolRequest {
            call_id: call_id.clone(),
            tool_name: ToolName::new("write"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            agent_id: crate::parse_agent_id("agent-1"),
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("reject reused extension call id");

    assert_eq!(h.tool_agents.get(&call_id), Some(&cid));
    assert_eq!(
        h.pending_tools.get(&call_id).map(|tool| tool.name.as_str()),
        Some("read")
    );
    assert_eq!(
        h.pending_tool_providers
            .get(&call_id)
            .map(tau_proto::ConnectionId::as_str),
        Some("owner-ext")
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
    assert!(
        !default_agent_tree(&h)
            .nodes()
            .iter()
            .any(|node| matches!(node.entry, AgentEntry::ToolResults { .. }))
    );

    h.handle_extension_event(
        "owner-ext",
        TestProtocolItem::Event(Event::ToolResult(ToolResult {
            call_id: call_id.clone(),
            tool_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
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
        h.prompt_agents.insert("sp-old".into(), cid.clone());
        h.handle_provider_response_finished(ProviderResponseFinished {
            agent_prompt_id: "sp-old".into(),
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
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
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
    h.prompt_agents.insert("sp-new".into(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-new".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
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
    h.registry.register(
        "drop-ext",
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
    h.agents
        .get_mut(&cid)
        .expect("agent")
        .pending_prompts
        .push_back(PendingPrompt::user("run".to_owned()));

    h.handle_disconnect("drop-ext");

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
    h.registry.register(
        "init-ext",
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
    h.turn_state = TurnState::InitializingSession {
        session_id: h.current_session_id.clone(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: HashSet::from([tau_proto::ConnectionId::from("init-ext")]),
    };
    let cid = ensure_test_user_agent(&mut h);
    h.agents
        .get_mut(&cid)
        .expect("agent")
        .pending_prompts
        .push_back(PendingPrompt::user("run".to_owned()));

    h.handle_disconnect("init-ext");

    assert!(h.turn_state.is_idle());
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

#[test]
fn non_tool_extension_query_tool_call_gets_terminal_error_before_teardown() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
    {
        let conv = h.agents.get_mut(&cid).expect("agent");
        conv.originator = tau_proto::PromptOriginator::Extension {
            name: "query-ext".into(),
            query_id: "query-1".into(),
        };
        conv.source_connection = Some(HARNESS_CONNECTION_ID.into());
        conv.parent_tool_call_id = None;
    }
    seed_agent_thinking(&mut h, &cid, "sp-query");
    h.prompt_agents.insert("sp-query".into(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-query".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "query-ext".into(),
            query_id: "query-1".into(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("non-tool query tool call terminalized");

    let tree = h
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

#[test]
fn non_tool_extension_query_pending_message_still_terminalizes_tool_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let durable_agent_id = durable_agent_id_for_conversation(&h, &cid);
    {
        let conv = h.agents.get_mut(&cid).expect("agent");
        conv.originator = tau_proto::PromptOriginator::Extension {
            name: "query-ext".into(),
            query_id: "query-2".into(),
        };
        conv.source_connection = Some(HARNESS_CONNECTION_ID.into());
        conv.parent_tool_call_id = None;
        conv.pending_prompts
            .push_back(PendingPrompt::agent_message_received("notice".to_owned()));
    }
    seed_agent_thinking(&mut h, &cid, "sp-query-pending");
    h.prompt_agents
        .insert("sp-query-pending".into(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-query-pending".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "query-ext".into(),
            query_id: "query-2".into(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("pending-message branch terminalizes tool call");

    let tree = h
        .agent_store
        .agent(durable_agent_id.as_str())
        .expect("agent tree remains");
    assert!(tree.nodes().iter().any(|node| matches!(
        &node.entry,
        AgentEntry::ToolResults { items }
            if items.iter().any(|item| item.call_id == "invalid_tool_call_sp-query-pending_1"
                && matches!(item.status, ToolResultStatus::Error { .. }))
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn non_tool_stop_reason_tool_call_gets_terminal_error() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-length");
    h.prompt_agents.insert("sp-length".into(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-length".into(),
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
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("length stop tool call terminalized");

    assert!(default_agent_tree(&h).nodes().iter().any(|node| matches!(
        &node.entry,
        AgentEntry::ToolResults { items }
            if items.iter().any(|item| item.call_id == "invalid_tool_call_sp-length_1"
                && matches!(item.status, ToolResultStatus::Error { .. }))
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn disconnect_removes_extension_prompt_and_agent_context() {
    let tmp = TempDir::new().expect("temp dir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_test_tool(&mut h, "ctx-ext");
    let contributor = tau_proto::ConnectionId::from("ctx-ext");
    let agent_id = crate::parse_agent_id("agent-1");

    h.publish_extension_prompt_fragment(
        "ctx-ext",
        tau_proto::ExtPromptFragmentPublish {
            fragment: tau_proto::PromptFragment::new(
                "ctx-fragment",
                tau_proto::PromptPriority::new(100),
                "stale fragment",
            ),
        },
    );
    h.publish_agent_context_publish(
        "ctx-ext",
        tau_proto::ExtAgentContextPublish {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentContextKey::from("skills"),
            value: tau_proto::AgentContextValue(serde_json::json!(["stale"])),
        },
    );
    h.agent_context_providers.insert(contributor.clone());
    h.pending_agent_context_ready
        .insert(agent_id.clone(), HashSet::from([contributor.clone()]));

    h.handle_disconnect("ctx-ext");

    assert!(!h.extension_prompt_fragments.contains_key(&contributor));
    assert_eq!(
        h.agent_context.template_value(Some(&agent_id)),
        serde_json::json!({})
    );
    assert!(!h.agent_context_providers.contains(&contributor));
    assert!(!h.pending_agent_context_ready.contains_key(&agent_id));
}

#[test]
fn switch_session_clears_session_scoped_extension_context() {
    let tmp = TempDir::new().expect("temp dir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_test_tool(&mut h, "ctx-ext");
    let contributor = tau_proto::ConnectionId::from("ctx-ext");
    let agent_id = crate::parse_agent_id("agent-1");

    h.publish_extension_prompt_fragment(
        "ctx-ext",
        tau_proto::ExtPromptFragmentPublish {
            fragment: tau_proto::PromptFragment::new(
                "ctx-fragment",
                tau_proto::PromptPriority::new(100),
                "old session fragment",
            ),
        },
    );
    h.publish_agent_context_publish(
        "ctx-ext",
        tau_proto::ExtAgentContextPublish {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentContextKey::from("skills"),
            value: tau_proto::AgentContextValue(serde_json::json!(["old session"])),
        },
    );
    h.agent_context_providers.insert(contributor.clone());
    h.pending_agent_context_ready
        .insert(agent_id.clone(), HashSet::from([contributor.clone()]));

    h.switch_session("s2".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");

    assert!(h.extension_prompt_fragments.contains_key(&contributor));
    let (fragments, tool_fragments) = h.gather_sourced_prompt_fragment_groups(&h.selected_role);
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
        h.agent_context.template_value(Some(&agent_id)),
        serde_json::json!({})
    );
    assert!(h.pending_agent_context_ready.is_empty());
    assert!(h.agent_context_providers.contains(&contributor));
}
