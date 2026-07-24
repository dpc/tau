use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::ErrorKind;

use super::*;
use crate::agent::PendingPrompt;
use crate::event::SUPERVISED_CLEANUP_GRACE;
use crate::extension::{
    ExtensionConnectCommand, ExtensionEntry, ExtensionState, spawn_in_process, spawn_supervised,
};
use crate::harness::{
    EXTENSION_RESTART_DELAY, MAX_EXTENSION_RESTART_ATTEMPTS, MAX_EXTENSION_RESTART_NOTICE_BYTES,
    PendingTool, PendingUiShellCommand, PromptFragmentSource, UiShellRouteId,
    extension_disconnected_tool_call_error_message, extension_restart_disabled_notice,
    prompt_snapshot_tool_error_message, tool_available_again_notice_prompt,
    tool_unavailable_notice_prompt, unavailable_tool_error_message, validate_protocol_version,
};
use crate::settings::ExtensionConfig;

/// The synchronous harness fault seam preserves rollback-poison lifecycle
/// behavior; direct writer tests own production singleton-poison coverage.
#[test]
fn synchronous_debug_log_poison_prevents_reenable() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut harness = echo_harness(td.path()).expect("harness");
    harness
        .debug_log
        .as_mut()
        .expect("durable harness debug log")
        .inject_rollback_failure();

    harness.log_event(&HarnessEvent::Disconnected {
        connection_id: tau_proto::ConnectionId::from("conn-1"),
    });

    assert!(harness.debug_log_poisoned);
    assert!(harness.debug_log.is_none());
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

fn supervised_test_config(name: &str, script: &str) -> ExtensionConfig {
    ExtensionConfig {
        tool_prefix: None,
        name: name.to_owned(),
        command: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
        role: None,
        require: true,
        cwd: None,
        config: serde_json::json!({}),
        secrets: BTreeMap::new(),
    }
}

fn connect_supervised_test_process(
    h: &mut Harness,
    config: ExtensionConfig,
    kind: tau_proto::ClientKind,
) -> (tau_proto::ConnectionId, u32) {
    let spawned = spawn_supervised(&config, kind.clone(), None, &h.tx)
        .expect("spawn supervised test process");
    let connection_id = spawned.connection_id.clone();
    let child_pid = spawned.child_pid;
    h.connect_extension(ExtensionConnectCommand {
        entry: ExtensionEntry {
            tool_prefix: config.tool_prefix.clone(),
            name: config.name.clone(),
            instance_id: 700.into(),
            connection_id: connection_id.clone(),
            kind,
            peer_capabilities: Default::default(),
            require: config.require,
            respawn_allowed: true,
            pid: Some(child_pid),
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
    .expect("connect supervised test process");
    (connection_id, child_pid)
}

fn drive_crashed_extension_cleanup(
    h: &mut Harness,
    extension_name: &str,
    now: Instant,
) -> tau_proto::ConnectionId {
    let started = Instant::now();
    loop {
        if started.elapsed() >= Duration::from_secs(3) {
            panic!("timed out waiting for crashed extension cleanup");
        }
        if let Some((connection_id, entry)) = h
            .extensions
            .entries
            .iter()
            .find(|(_, entry)| entry.name == extension_name)
            && entry.state == ExtensionState::Disconnected
            && !h.extensions.supervised_writers.contains_key(connection_id)
            && (entry.kind == tau_proto::ClientKind::Provider
                || !entry.respawn_allowed
                || h.extensions.restart_deadlines.contains_key(connection_id))
        {
            return connection_id.clone();
        }

        let event =
            h.rx.recv_timeout(Duration::from_secs(1))
                .expect("extension lifecycle event");
        match event {
            HarnessEvent::FromConnection {
                connection_id,
                message,
            } => h
                .handle_extension_message(&connection_id, *message)
                .expect("extension message"),
            HarnessEvent::Disconnected { connection_id }
            | HarnessEvent::ReadFailed { connection_id, .. } => {
                h.handle_disconnect_at(&connection_id, now)
            }
            HarnessEvent::SupervisedWriterCleanupComplete { connection_id } => h
                .handle_supervised_writer_cleanup_complete_at(&connection_id, now)
                .expect("supervised writer cleanup"),
            HarnessEvent::Command(command) => {
                h.handle_harness_command(command).expect("harness command");
            }
            HarnessEvent::NewClient(_) => {}
        }
    }
}

fn process_is_signalable(pid: u32) -> bool {
    // SAFETY: signal 0 checks process existence/permission without delivering
    // a signal.
    #[allow(unsafe_code)]
    unsafe {
        libc::kill(pid as libc::pid_t, 0) == 0
    }
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
        TestProtocolItem::Event(Event::ToolUnregistrationDeclared(
            tau_proto::ToolUnregistrationDeclared {
                tool_name: ToolName::new("shell"),
            },
        )),
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
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: spec,
                tool_group: None,
                prompt_fragment: None,
            },
        )),
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

fn staged_invalid_tool_spec(name: &str) -> ToolSpec {
    let mut spec = staged_tool_spec(name);
    spec.examples.push(tau_proto::ToolExample {
        id: String::new(),
        title: None,
        arguments: CborValue::Map(Vec::new()),
        note: None,
        subcommand: None,
    });
    spec
}

fn staged_provider_model(id: &str) -> tau_proto::ProviderModelInfo {
    tau_proto::ProviderModelInfo {
        id: id.into(),
        display_name: Some("Staged".to_owned()),
        tags: Vec::new(),
        supported_tool_types: vec![],
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: true,
        default_affinity: 100,
        context_window: 4_096,
        efforts: vec![tau_proto::Effort::Medium],
        verbosities: vec![tau_proto::Verbosity::Medium],
        thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_threshold: None,
        est_uncached_input_cost_1m_usd: Default::default(),
        est_cached_input_cost_1m_usd: Default::default(),
        est_output_cost_1m_usd: Default::default(),
    }
}

fn clear_quiet_provider_models(h: &mut Harness) {
    let provider_id = h
        .extension_connection_id("provider")
        .expect("provider")
        .to_owned();
    h.handle_extension_event(
        &provider_id,
        TestProtocolItem::Event(Event::ProviderModelsDeclared(
            tau_proto::ProviderModelsDeclared { models: Vec::new() },
        )),
    )
    .expect("clear provider models");
}

/// Seeds the valid durable success boundary required before a restored
/// standalone continuation can enter `AwaitingCheckpoint`.
pub(super) fn seed_restored_compaction_checkpoint(
    h: &mut Harness,
    cid: &AgentId,
    model: &tau_proto::ModelId,
    transaction: &str,
) -> (
    tau_proto::AgentId,
    tau_proto::CompactionTransactionId,
    tau_proto::AgentPromptId,
    tau_proto::AgentHead,
) {
    let agent_id = crate::parse_agent_id(h.agents[cid].agent_id.as_deref().expect("durable agent"));
    let transaction_id =
        tau_proto::CompactionTransactionId::parse(transaction).expect("transaction id");
    let compact_prompt_id = tau_proto::AgentPromptId::from(format!("ap-{transaction}-compact"));
    let started = tau_proto::AgentStandaloneCompactionStarted {
        agent_id: agent_id.clone(),
        transaction_id: transaction_id.clone(),
        compact_prompt_id: compact_prompt_id.clone(),
        cut: tau_proto::AgentHead::Root,
        resume_through: Some(tau_proto::AgentHead::Root),
        model: model.clone(),
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        originator: tau_proto::PromptOriginator::User,
        supersedes: None,
        trigger: tau_proto::StandaloneCompactionTrigger::Manual,
    };
    for event in [
        Event::AgentStandaloneCompactionStarted(started),
        Event::AgentCompacted(tau_proto::AgentCompacted {
            agent_id: agent_id.clone(),
            transaction_id: Some(transaction_id.clone()),
            cut: Some(tau_proto::AgentHead::Root),
            suffix_end: Some(tau_proto::AgentHead::Root),
            compact_prompt_id: Some(compact_prompt_id),
            model: Some(model.clone()),
            operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
            replacement_window: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "durable restored summary".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
        }),
    ] {
        h.agent_store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                tau_core::AgentEventParent::Root,
                event,
                tau_proto::UnixMicros::now(),
            )
            .expect("seed valid durable compaction outcome");
    }
    let recovery = h
        .agent_store
        .agent(agent_id.as_str())
        .and_then(tau_core::AgentTree::standalone_compaction_recovery)
        .expect("core recovery projection");
    let tau_core::StandaloneCompactionRecovery::AwaitingCheckpoint { through, .. } = &recovery
    else {
        panic!("successful resumable compaction awaits its checkpoint");
    };
    h.agents.get_mut(cid).expect("runtime agent").head = through.as_option();
    let checkpoint_prompt_id = h
        .stage_restored_compaction_recovery(cid, &recovery)
        .expect("production runtime recovery projection");
    (agent_id, transaction_id, checkpoint_prompt_id, *through)
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
            tool_prefix: None,
            name: conn_id.to_owned(),
            instance_id: 42.into(),
            connection_id: connection_id.clone(),
            kind,
            peer_capabilities: Default::default(),
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
            tool_prefix: None,
            name: name.to_owned(),
            instance_id: 42.into(),
            connection_id: connection_id.clone(),
            kind: tau_proto::ClientKind::Tool,
            peer_capabilities: Default::default(),
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

fn debug_event_stats_request(extension_name: &str) -> HarnessInputMessage {
    HarnessInputMessage::UiDebugEventStatsRequest(tau_proto::UiDebugEventStatsRequest {
        extension_name: extension_name.into(),
    })
}

fn detach_request() -> HarnessInputMessage {
    HarnessInputMessage::UiDetachRequest(tau_proto::UiDetachRequest {})
}

fn tree_request(session_id: &str, target_agent_id: Option<&str>) -> HarnessInputMessage {
    HarnessInputMessage::UiTreeRequest(tau_proto::UiTreeRequest {
        session_id: session_id.into(),
        target_agent_id: target_agent_id.map(crate::parse_agent_id),
    })
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
    append_user_message_via_event(&mut h, "s1", "first tree prompt");
    append_user_message_via_event(&mut h, "s1", "second tree prompt");
    let (requesting_ui_id, mut requesting_ui) = connect_socket_ui(&mut h);
    let (_observer_id, mut observer) = connect_socket_ui(&mut h);
    let debug_log_path = h
        .enable_debug_log(&td.path().join("debug"))
        .expect("enable debug log");
    let baseline_seq = h.event_log.next_seq();
    let event = HarnessEvent::FromConnection {
        connection_id: requesting_ui_id,
        message: Box::new(tree_request("s1", Some(agent_id.as_str()))),
    };
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
    assert!(!notice.always_show);
    assert_eq!(
        notice.message,
        concat!(
            "    0   before first prompt (root)\n",
            "    1   before prompt  user: first tree prompt\n",
            "    2   before prompt  user: second tree prompt",
        )
    );
    assert_no_message(&mut requesting_ui);
    assert_no_message(&mut observer);
    assert_eq!(h.event_log.next_seq(), baseline_seq);

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
    let baseline_seq = h.event_log.next_seq();

    assert!(
        !h.handle_startup_from_connection(ui_id.as_str(), tree_request("s1", None))
            .expect("handle startup tree request")
    );

    let notice = read_notice(&mut ui);
    assert_eq!(notice.message, "tree request ignored: unknown agent");
    assert_no_message(&mut ui);
    assert_eq!(h.event_log.next_seq(), baseline_seq);
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
        external_id.as_str(),
        HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME.into(),
            client_kind: tau_proto::ClientKind::External,
            capabilities: Default::default(),
        }),
    )
    .expect("external-agent message hello");
    let baseline_seq = h.event_log.next_seq();

    for connection_id in [
        tau_proto::ConnectionId::from("tree-socket-tool"),
        tau_proto::ConnectionId::from("tree-embedded-ui"),
        external_id,
    ] {
        let mut served_clients = 0;
        let mut exit_on_disconnect = false;
        let mut ever_attached = false;
        h.handle_runtime_event(
            HarnessEvent::FromConnection {
                connection_id,
                message: Box::new(tree_request("s1", None)),
            },
            &mut served_clients,
            &mut exit_on_disconnect,
            &mut ever_attached,
        )
        .expect("silently deny tree request");
        assert_eq!(served_clients, 0);
    }

    assert_eq!(h.event_log.next_seq(), baseline_seq);
    assert!(socket_tool.lock().expect("socket tool frames").is_empty());
    assert!(embedded_ui.lock().expect("embedded UI frames").is_empty());
    assert_no_message(&mut external);
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
    let notice_count = h.replayable_harness_notices.len();

    for connection_id in ["tree-ready-requester", "tree-handshaking-requester"] {
        h.handle_extension_message(connection_id, tree_request("s1", None))
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
    assert_eq!(h.replayable_harness_notices.len(), notice_count);
}

/// Tree requests preserve ordinary configured-extension phase validation:
/// pre-Hello requests are metered and then follow runtime protocol-failure
/// isolation instead of the legal-phase silent denial.
#[test]
fn tree_request_preserves_configured_extension_phase_validation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    h.initial_extension_tool_preflight_complete = true;
    connect_handshaking_tool(&mut h, "tree-spawning-requester");
    h.extensions
        .entries
        .get_mut("tree-spawning-requester")
        .expect("spawning requester")
        .state = ExtensionState::Spawning;
    let notice_count = h.replayable_harness_notices.len();

    h.handle_extension_message("tree-spawning-requester", tree_request("s1", None))
        .expect("isolate out-of-phase requester");

    let entry = &h.extensions.entries["tree-spawning-requester"];
    assert_eq!(
        entry.protocol_io.cumulative_stats().uplink["message.ui_tree_request"].count,
        1
    );
    assert_eq!(entry.state, ExtensionState::Disconnected);
    assert!(h.bus.connection("tree-spawning-requester").is_none());
    assert_eq!(h.replayable_harness_notices.len(), notice_count + 1);
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
    let baseline_seq = h.event_log.next_seq();
    let event = HarnessEvent::FromConnection {
        connection_id: ui_id,
        message: Box::new(detach_request()),
    };
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
    assert_eq!(h.event_log.next_seq(), baseline_seq);
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
        !h.handle_startup_from_connection("socket-tool", detach_request())
            .expect("deny socket tool detach")
    );
    assert!(!h.startup_detach_requested);

    let (ui_id, mut ui) = connect_socket_ui(&mut h);
    assert!(
        !h.handle_startup_from_connection(ui_id.as_str(), detach_request())
            .expect("handle attached UI detach")
    );
    assert!(h.startup_detach_requested);
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
        external_id.as_str(),
        HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME.into(),
            client_kind: tau_proto::ClientKind::External,
            capabilities: Default::default(),
        }),
    )
    .expect("external-agent message hello");
    let baseline_seq = h.event_log.next_seq();

    for connection_id in [
        tau_proto::ConnectionId::from("socket-tool"),
        tau_proto::ConnectionId::from("embedded-ui"),
        external_id,
    ] {
        let mut served_clients = 0;
        let mut exit_on_disconnect = true;
        let mut ever_attached = false;
        h.handle_runtime_event(
            HarnessEvent::FromConnection {
                connection_id,
                message: Box::new(detach_request()),
            },
            &mut served_clients,
            &mut exit_on_disconnect,
            &mut ever_attached,
        )
        .expect("silently deny detach request");
        assert!(exit_on_disconnect);
        assert_eq!(served_clients, 0);
    }

    assert_eq!(h.event_log.next_seq(), baseline_seq);
    assert!(socket_tool.lock().expect("socket tool frames").is_empty());
    assert!(embedded_ui.lock().expect("embedded UI frames").is_empty());
    assert_no_message(&mut external);
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
    let notice_count = h.replayable_harness_notices.len();

    for connection_id in ["ready-requester", "handshaking-requester"] {
        h.handle_extension_message(connection_id, detach_request())
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
    assert!(h.bus.connection("ready-requester").is_some());
    assert!(h.bus.connection("handshaking-requester").is_some());
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
    assert_eq!(h.replayable_harness_notices.len(), notice_count);
}

/// Silent configured-extension denial happens only after ordinary phase
/// validation: a detach request before Hello is metered, then follows normal
/// runtime protocol-failure isolation.
#[test]
fn detach_request_preserves_configured_extension_phase_validation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    h.initial_extension_tool_preflight_complete = true;
    connect_handshaking_tool(&mut h, "spawning-requester");
    h.extensions
        .entries
        .get_mut("spawning-requester")
        .expect("spawning requester")
        .state = ExtensionState::Spawning;
    let notice_count = h.replayable_harness_notices.len();

    h.handle_extension_message("spawning-requester", detach_request())
        .expect("isolate out-of-phase requester");

    let entry = &h.extensions.entries["spawning-requester"];
    assert_eq!(
        entry.protocol_io.cumulative_stats().uplink["message.ui_detach_request"].count,
        1
    );
    assert_eq!(entry.state, ExtensionState::Disconnected);
    assert!(h.bus.connection("spawning-requester").is_none());
    assert_eq!(h.replayable_harness_notices.len(), notice_count + 1);
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
        HarnessEvent::FromConnection {
            connection_id: requesting_ui_id,
            message: Box::new(debug_event_stats_request("std-shell")),
        },
        &mut served_clients,
        &mut exit_on_disconnect,
        &mut ever_attached,
    )
    .expect("request stats through runtime router");

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
    h.extensions
        .entries
        .get_mut("std-shell")
        .expect("extension")
        .state = ExtensionState::Spawning;

    h.handle_extension_event(
        "std-shell",
        TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "std-shell".into(),
            client_kind: tau_proto::ClientKind::Tool,
            capabilities: Default::default(),
        })),
    )
    .expect("extension hello");
    h.handle_client_message(ui_id.as_str(), debug_event_stats_request("std-shell"))
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
        HarnessEvent::FromConnection {
            connection_id: "ui".into(),
            message: Box::new(debug_event_stats_request("secret-ext")),
        },
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
    assert!(notice.always_show);
    assert_eq!(
        notice.message,
        "extension event stats are only available to attached local UIs"
    );
    assert!(
        other_ui.lock().expect("other UI frames").is_empty(),
        "denial must not leak to another peer"
    );
}

/// A socket peer dedicated to external-agent messaging cannot use its initial
/// UI classification to read extension protocol counters.
#[test]
fn debug_event_stats_request_rejects_dedicated_external_peer_without_leaking_counters() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let (client_id, mut client) = connect_socket_ui(&mut h);
    let (_other_ui_id, mut other_ui) = connect_socket_ui(&mut h);
    h.handle_client_message(
        client_id.as_str(),
        HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME.into(),
            client_kind: tau_proto::ClientKind::External,
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

    h.handle_client_message(client_id.as_str(), debug_event_stats_request("secret-ext"))
        .expect("request stats");

    let notice = read_notice(&mut client);
    assert_eq!(notice.kind, tau_proto::notice_kind::UI_COMMAND_ERROR);
    assert_eq!(
        notice.message,
        "extension event stats are only available to attached local UIs"
    );
    assert_no_message(&mut client);
    assert_no_message(&mut other_ui);
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

    let notice_count = h.replayable_harness_notices.len();
    let event = HarnessEvent::FromConnection {
        connection_id: "requester".into(),
        message: Box::new(debug_event_stats_request("secret-ext")),
    };
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
        h.replayable_harness_notices.len(),
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
    let notice_count = h.replayable_harness_notices.len();
    let event = HarnessEvent::FromConnection {
        connection_id: "requester".into(),
        message: Box::new(debug_event_stats_request("secret-ext")),
    };
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
    assert!(h.bus.connection("requester").is_some());
    assert!(
        !h.extensions.activation_staging.contains_key("requester"),
        "denied request must not consume activation quota"
    );
    assert_eq!(h.replayable_harness_notices.len(), notice_count);
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
        !h.handle_startup_from_connection(
            ui_id.as_str(),
            debug_event_stats_request("missing-extension"),
        )
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

    h.handle_client_message(ui_id.as_str(), debug_event_stats_request("std-shell"))
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

    h.handle_client_message(ui_id.as_str(), debug_event_stats_request("std-shell"))
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
    Event::ToolResultReported(ToolResult {
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("ok".to_owned()),
        provider_content: Vec::new(),
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
    h.extensions
        .entries
        .get_mut("std-email")
        .expect("extension")
        .state = ExtensionState::Spawning;

    h.handle_extension_event(
        "std-email",
        TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "tau-ext-pim".into(),
            client_kind: tau_proto::ClientKind::Tool,
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
            client_name: "tau-ext-pim".into(),
            client_kind: tau_proto::ClientKind::Tool,
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
    entry.name = configured_name.to_owned();
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
        publisher_extension_id: tau_proto::MessagePublisherId::new("forged"),
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
        .into_stamped_canonical_message_fact(tau_proto::MessagePublisherId::new(configured_name))
        .expect("authenticated message fact");
    assert!(matches!(
        canonical,
        Event::MessageDelivered(prepared)
            if prepared.publisher_extension_id.as_str() == configured_name
    ));

    h.handle_extension_event_inner_with_persist(
        conn_id,
        Event::MessageDeliveredReported(report),
        Some(false),
    )
    .expect("report intake");

    assert!(matches!(
        h.store
            .session_events("s1")
            .expect("durable fallback journal")
            .as_slice(),
        [tau_core::PersistedSessionEvent {
            source: Some(source),
            event: Event::MessageDelivered(fact),
            ..
        }] if source.as_str() == HARNESS_CONNECTION_ID
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
                if report.publisher_extension_id.as_str() == "forged"
        )
    }));
    let events = event_log_events(&h);
    let report_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::MessageDeliveredReported(report)
                    if report.publisher_extension_id.as_str() == "forged"
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

/// A configured extension cannot bypass report processing by directly emitting
/// a harness-owned canonical message fact.
#[test]
fn extension_cannot_emit_canonical_message_fact() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    connect_ready_message_publisher(&mut h, "bridge", "configured-bridge");
    let canonical = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("forged"),
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

    h.handle_extension_event_inner_with_persist("bridge", canonical, Some(true))
        .expect("extension intake");

    assert!(
        h.store
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

/// A socket client cannot claim report or canonical message publication
/// authority.
#[test]
fn socket_client_cannot_emit_message_reports_or_canonical_facts() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let client_id = "ui";
    connect_test_client(&mut h, client_id, tau_proto::ClientKind::Ui);
    let fact = Event::MessageDelivered(tau_proto::MessageDelivered {
        publisher_extension_id: tau_proto::MessagePublisherId::new("forged"),
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

    h.handle_client_event_inner(client_id, fact)
        .expect("client intake");
    h.handle_client_event_inner(
        client_id,
        Event::MessageDeliveredReported(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::new("forged"),
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
            connection_id,
            Event::MessageDeliveredReported(tau_proto::MessageDelivered::new(
                tau_proto::MessagePublisherId::new("forged"),
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
    h.initial_extension_tool_preflight_complete = false;
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
    assert!(h.registry.providers_for("too_early").is_empty());

    let ready_error = h
        .handle_extension_message(conn_id, TestMessage::Ready(Default::default()))
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
    h.initial_extension_tool_preflight_complete = false;
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
fn required_initial_config_error_emits_diagnostic_then_fails_startup() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.initial_extension_tool_preflight_complete = false;
    let conn_id = "required-config-bad-ext";
    connect_handshaking_tool(&mut h, conn_id);

    let error = h
        .handle_extension_message(
            conn_id,
            TestMessage::ConfigError(tau_proto::ConfigError {
                message: "required setting is invalid".to_owned(),
            }),
        )
        .expect_err("required initial config rejection is startup-fatal");

    assert!(error.to_string().contains("required extension"));
    assert!(event_log_contains_source_event(
        &h,
        "harness",
        |event| matches!(
            event,
            Event::HarnessNotice(notice)
                if notice.kind == tau_proto::notice_kind::EXTENSION_CONFIG_ERROR
                    && notice.message.contains("required setting is invalid")
        )
    ));
    h.shutdown().expect("shutdown");
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
        core: crate::settings::CoreConfig {
            mode: crate::settings::CoreMode::Embedded,
        },
        extensions: BTreeMap::from([(
            extension_name.clone(),
            crate::settings::ExtensionConfig {
                tool_prefix: None,
                name: extension_name.clone(),
                command,
                args: vec!["--token=argument-secret".to_owned()],
                role: None,
                require: false,
                cwd: None,
                config: serde_json::json!({"token": "config-secret"}),
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

    assert!(h.extension_connection_id(&extension_name).is_none());
    assert!(event_log_contains_source_event(
        &h,
        "harness",
        |event| matches!(
            event,
            Event::HarnessNotice(info)
                if info.level == tau_proto::NoticeLevel::Warning
                    && info.kind == tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED
                    && info.always_show
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

/// Concrete provider model metadata, rather than a manually assembled template
/// context, controls parallel-tool guidance in normal and preview rendering.
#[test]
fn provider_model_parallel_capability_flows_into_prompt_rendering() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let model: tau_proto::ModelId = "test/model".parse().expect("model id");
    let mut info = staged_provider_model("test/model");
    info.supports_parallel_tool_calls = false;
    h.provider_model_info.insert(model.clone(), info);
    h.selected_model = Some(model);

    let normal = h.build_system_prompt_for_role(&h.selected_role);
    let preview = h
        .build_system_prompt_for_role_preview(&h.selected_role)
        .expect("preview prompt");

    assert!(normal.contains("at most one tool call"));
    assert!(preview.contains("at most one tool call"));
    assert!(!normal.contains("Maximize use of parallel tool calls"));
    assert!(!preview.contains("Maximize use of parallel tool calls"));
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

/// The initial barrier activates two simultaneously configured Slack-style
/// instances only after both are ready, routes each final name to its exact
/// owner, and preserves the survivor when one disconnects.
#[test]
fn two_prefixed_instances_coexist_and_disconnect_independently() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.initial_extension_tool_preflight_complete = false;
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
            connection_id,
            TestMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: connection_id.to_owned().into(),
                client_kind: tau_proto::ClientKind::Tool,
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

    h.handle_extension_message("slack-work", TestMessage::Ready(Default::default()))
        .expect("first ready");
    assert!(h.registry.providers_for("work_slack_send").is_empty());
    h.handle_extension_message("slack-personal", TestMessage::Ready(Default::default()))
        .expect("second ready");

    assert_eq!(
        h.registry.providers_for("personal_slack_send")[0]
            .connection_id
            .as_str(),
        "slack-personal"
    );
    assert_eq!(
        h.registry.providers_for("work_slack_send")[0]
            .connection_id
            .as_str(),
        "slack-work"
    );
    for (tool_name, owner, call_id) in [
        ("personal_slack_send", "slack-personal", "personal-call"),
        ("work_slack_send", "slack-work", "work-call"),
    ] {
        let route = h
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
            tau_core::ToolRouteTarget::Extension(owner.into())
        );
        h.bus
            .send_to(
                owner,
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
    h.handle_disconnect("slack-personal");
    assert!(h.registry.providers_for("personal_slack_send").is_empty());
    assert_eq!(
        h.registry.providers_for("work_slack_send")[0]
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
    h.initial_extension_tool_preflight_complete = false;
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

    h.handle_extension_message("ready-owner", TestMessage::Ready(Default::default()))
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
    assert!(h.registry.providers_for("barrier_tool").is_empty());
    assert!(
        h.registry
            .providers_for("post_ready_barrier_tool")
            .is_empty()
    );
    assert_eq!(
        h.extensions.entries["ready-owner"].state,
        ExtensionState::Handshaking
    );

    h.handle_startup_disconnect("optional-blocker")
        .expect("optional disconnect degrades");

    assert_eq!(
        h.extensions.entries["ready-owner"].state,
        ExtensionState::Ready
    );
    assert_eq!(
        h.registry.providers_for("barrier_tool")[0]
            .connection_id
            .as_str(),
        "ready-owner"
    );
    assert_eq!(
        h.registry.providers_for("post_ready_barrier_tool")[0]
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
    h.initial_extension_tool_preflight_complete = false;
    let sink = connect_handshaking_tool(&mut h, "config-rpc");
    connect_handshaking_tool(&mut h, "activation-blocker");
    let request = |request_id: &str, op| {
        HarnessInputMessage::ExtensionDataRequest(tau_proto::ExtensionDataRequest {
            request_id: request_id.to_owned(),
            scope: tau_proto::ExtensionDataScope::User,
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
        "config-rpc",
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

    h.handle_extension_message("config-rpc", TestMessage::Ready(Default::default()))
        .expect("peer Ready waits on blocker");
    h.handle_extension_message(
        "config-rpc",
        request(
            "post-ready-read",
            tau_proto::ExtensionDataRequestOp::ReadFile {
                path: tau_proto::ExtensionDataPath::from("state.json"),
            },
        ),
    )
    .expect("post-Ready request is deferred");
    assert!(!has_result("post-ready-read"));

    h.handle_extension_message("activation-blocker", TestMessage::Ready(Default::default()))
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
        h.initial_extension_tool_preflight_complete = false;
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
        h.handle_extension_message("late-claimant", TestMessage::Ready(Default::default()))
            .expect("claimant Ready");
        if !blocked_at_registration {
            h.handle_extension_message("startup-owner", TestMessage::Ready(Default::default()))
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
            h.handle_extension_message("startup-owner", TestMessage::Ready(Default::default()))
                .expect("release initial barrier");
        }
        assert_eq!(
            h.registry.providers_for("ready_frozen_tool")[0]
                .connection_id
                .as_str(),
            "startup-owner"
        );
        assert_eq!(
            h.extensions.entries["late-claimant"].state,
            ExtensionState::Ready
        );
        assert_eq!(h.registry.providers_for("ready_frozen_tool").len(), 1);
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
    h.initial_extension_tool_preflight_complete = false;
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
    h.handle_extension_message("ready-owner", TestMessage::Ready(Default::default()))
        .expect("ready received");

    h.handle_startup_disconnect("required-tool-blocker")
        .expect("required tool compatibility disconnect");

    assert_eq!(
        h.extensions.entries["ready-owner"].state,
        ExtensionState::Ready
    );
    assert_eq!(
        h.registry.providers_for("required_disconnect_barrier_tool")[0]
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
    h.initial_extension_tool_preflight_complete = false;
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
    h.handle_extension_message("required-ready", TestMessage::Ready(Default::default()))
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
        h.registry.providers_for("timeout_barrier_tool")[0]
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
    h.initial_extension_tool_preflight_complete = false;

    h.wait_for_extensions_ready().expect("empty barrier");

    assert!(h.initial_extension_tool_preflight_complete);
    h.shutdown().expect("shutdown");
}

/// Protocol-version mismatches preserve required/optional extension
/// availability policy.
#[test]
fn optional_mismatched_protocol_is_disabled_but_required_mismatch_is_fatal() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.initial_extension_tool_preflight_complete = false;
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
            client_name: name.to_owned().into(),
            client_kind: tau_proto::ClientKind::Tool,
            capabilities: Default::default(),
        })
    };

    h.handle_extension_message("optional-old", mismatched_hello("optional-old"))
        .expect("optional mismatched peer is disabled");
    assert_eq!(
        h.extensions.entries["optional-old"].state,
        ExtensionState::Disconnected
    );
    let error = h
        .handle_extension_message("required-old", mismatched_hello("required-old"))
        .expect_err("required mismatched peer is fatal");
    assert!(error.to_string().contains("protocol"));
    h.shutdown().expect("shutdown");
}

/// Reader decode failures retain their provenance through startup and runtime
/// policy instead of being mistaken for compatibility EOF disconnects.
#[test]
fn decode_failures_follow_required_optional_and_runtime_policy() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.initial_extension_tool_preflight_complete = false;
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

    h.handle_startup_read_failure("optional-decode", "malformed cbor".to_owned())
        .expect("optional decode failure degrades");
    assert_eq!(
        h.extensions.entries["optional-decode"].state,
        ExtensionState::Disconnected
    );
    assert!(!h.extensions.entries["optional-decode"].respawn_allowed);

    let error = h
        .handle_startup_read_failure("required-decode", "oversized frame".to_owned())
        .expect_err("required initial decode failure is fatal");
    assert!(error.to_string().contains("protocol decode failed"));

    h.initial_extension_tool_preflight_complete = true;
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
            connection_id: "runtime-decode".into(),
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
    h.initial_extension_tool_preflight_complete = true;

    h.handle_extension_message("runtime-bad", TestMessage::Ready(Default::default()))
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
    h.initial_extension_tool_preflight_complete = true;

    h.handle_extension_message(
        "runtime-config-bad",
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

/// Pre-activation storage is bounded by both retained frame count and encoded
/// bytes; optional overflow isolates the claimant instead of growing without
/// bound.
#[test]
fn optional_activation_staging_enforces_count_and_byte_quotas() {
    let make_harness = |connection_id: &str| {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        h.initial_extension_tool_preflight_complete = false;
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
    for _ in 0..super::super::MAX_EXTENSION_ACTIVATION_MESSAGES {
        diagnostic_harness
            .handle_extension_message("diagnostic-at-limit", subscribe())
            .expect("message within count limit");
    }
    diagnostic_harness
        .handle_extension_message(
            "diagnostic-at-limit",
            TestMessage::ConfigError(tau_proto::ConfigError {
                message: "configuration rejected at quota boundary".to_owned(),
            }),
        )
        .expect("mandatory config diagnostic bypasses retained-message quota");
    assert!(event_log_contains_source_event(
        &diagnostic_harness,
        "harness",
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
        .extension_activation_stage_mut("ready-at-limit")
        .retained_message_count = super::super::MAX_EXTENSION_ACTIVATION_MESSAGES;
    ready_harness
        .handle_extension_message("ready-at-limit", TestMessage::Ready(Default::default()))
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
            "oversized-diagnostic",
            TestMessage::ConfigError(tau_proto::ConfigError {
                message: "x".repeat(super::super::MAX_EXTENSION_ACTIVATION_BYTES + 1),
            }),
        )
        .expect("oversized config diagnostic is bounded and emitted");
    assert!(event_log_contains_source_event(
        &oversized_diagnostic_harness,
        "harness",
        |event| matches!(
            event,
            Event::HarnessNotice(notice)
                if notice.kind == tau_proto::notice_kind::EXTENSION_CONFIG_ERROR
                    && notice.message.contains("[truncated]")
                    && notice.message.len()
                        < super::super::MAX_EXTENSION_CONFIG_ERROR_BYTES * 2
        )
    ));
    oversized_diagnostic_harness.shutdown().expect("shutdown");

    for _ in 0..super::super::MAX_EXTENSION_ACTIVATION_MESSAGES {
        count_harness
            .handle_extension_message("count-overflow", subscribe())
            .expect("message within count limit");
    }
    count_harness
        .handle_extension_message("count-overflow", subscribe())
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
                "x".repeat(super::super::MAX_EXTENSION_ACTIVATION_BYTES + 1),
            )],
        })
    };
    let (_bytes_td, mut bytes_harness) = make_harness("bytes-overflow");
    bytes_harness
        .handle_extension_message("bytes-overflow", oversized())
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
        .handle_extension_message("required-overflow", oversized())
        .expect_err("required initial overflow is fatal");
    required_harness.shutdown().expect("shutdown");

    let (_runtime_td, mut runtime_harness) = make_harness("runtime-overflow");
    runtime_harness.initial_extension_tool_preflight_complete = true;
    runtime_harness
        .handle_extension_message("runtime-overflow", oversized())
        .expect("runtime overflow isolates");
    let runtime_entry = &runtime_harness.extensions.entries["runtime-overflow"];
    assert_eq!(runtime_entry.state, ExtensionState::Disconnected);
    assert!(runtime_entry.respawn_allowed);
    runtime_harness.shutdown().expect("shutdown");
}

/// Internal tool reservations are present before initial extension preflight:
/// optional conflicts degrade and required conflicts fail startup.
#[test]
fn initial_internal_tool_conflicts_follow_availability_policy() {
    let run = |required: bool| {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        h.initial_extension_tool_preflight_complete = false;
        h.registry
            .register_internal("harness", staged_tool_spec("reserved_tool"));
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
        let result = h.handle_extension_message("claimant", TestMessage::Ready(Default::default()));
        (h, result)
    };

    let (mut optional, result) = run(false);
    result.expect("optional conflict degrades");
    assert_eq!(
        optional.extensions.entries["claimant"].state,
        ExtensionState::Disconnected
    );
    assert_eq!(
        optional.registry.providers_for("reserved_tool")[0].kind,
        tau_core::ToolProviderKind::Internal
    );
    optional.shutdown().expect("shutdown");

    let (mut required, result) = run(true);
    assert!(result.is_err(), "required internal conflict is fatal");
    assert_eq!(
        required.registry.providers_for("reserved_tool")[0].kind,
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
        h.initial_extension_tool_preflight_complete = false;
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
            result =
                h.handle_extension_message(connection_id, TestMessage::Ready(Default::default()));
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
        h.initial_extension_tool_preflight_complete = false;
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
        h.handle_extension_message("invalid-owner", TestMessage::Ready(Default::default()))
            .expect("first Ready waits");
        let result =
            h.handle_extension_message("valid-owner", TestMessage::Ready(Default::default()));
        (h, result)
    };

    let (mut optional_invalid, result) = run(false);
    result.expect("invalid optional peer degrades");
    assert_eq!(
        optional_invalid
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
        h.initial_extension_tool_preflight_complete = false;
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
        let result =
            h.handle_extension_message("refresh-owner", TestMessage::Ready(Default::default()));
        (h, result)
    };

    let (mut valid_final, result) = run(
        staged_invalid_tool_spec("refreshed_tool"),
        staged_tool_spec("refreshed_tool"),
    );
    result.expect("valid final refresh wins");
    assert_eq!(
        valid_final.registry.providers_for("refreshed_tool").len(),
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
    h.selected_model = Some("test/model".into());
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

    let blocking_sink = connect_ready_configured_extension(
        &mut h,
        "conn-blocking-tool",
        "configured-blocking-tool",
        tau_proto::ClientKind::Tool,
    );
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
        context_limit_telemetry: None,
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
        context_limit_telemetry: None,
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
        TestProtocolItem::Event(Event::ProviderModelsDeclared(
            tau_proto::ProviderModelsDeclared {
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
        conn_id,
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("receive ready");
    assert_eq!(
        h.extensions.entries[conn_id].state,
        ExtensionState::Handshaking
    );
    assert!(h.extensions.ready_received.contains(conn_id));
    assert!(!h.provider_model_routes.contains_key(&model));

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
        h.pending_intercept.as_ref().map(|pending| &pending.event),
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
    assert!(!h.provider_model_routes.contains_key(&model));
    assert_eq!(
        h.provider_models_by_extension
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
    h.initial_extension_tool_preflight_complete = false;
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
    oversized.display_name = Some("x".repeat(super::super::MAX_EXTENSION_ACTIVATION_BYTES));
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
        !h.provider_model_routes
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
    h.initial_extension_tool_preflight_complete = false;
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
        h.handle_extension_message(source, TestMessage::Ready(Default::default()))
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
        h.registry
            .providers_for("intercept_blocked_collision")
            .is_empty()
    );
    assert_eq!(
        h.extensions.entries[provider].state,
        ExtensionState::Handshaking
    );
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
        .try_build_system_prompt_for_role_and_agent(
            &h.selected_role,
            Some(&prompt_agent_id),
            &[],
            None,
            false,
        )
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
        .try_build_system_prompt_for_role_and_agent(
            &h.selected_role,
            Some(&prompt_agent_id),
            &[],
            None,
            false,
        )
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

/// Ensures a committed AGENTS.md declaration remains projection-staged until
/// extension Ready, while operational per-agent readiness and queued prompts
/// stay behind activation and observe the activated instructions.
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
    assert!(event_log_contains_source_event(&h, conn_id, |event| {
        matches!(event, Event::ExtAgentsMdAvailable(_))
    }));
    assert!(!event_log_contains_source_event(&h, conn_id, |event| {
        matches!(event, Event::ExtensionContextReady(_))
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

/// Pre-Ready operational emits from multiple extensions remain globally
/// deferred and commit in wire order before a start-agent request creates work.
#[test]
fn extension_emit_and_start_agent_request_are_deferred_in_order_until_ready() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.initial_extension_tool_preflight_complete = false;
    let first_id = "ordering-first";
    let second_id = "ordering-second";
    let _first_sink = connect_handshaking_tool(&mut h, first_id);
    let _second_sink = connect_handshaking_tool(&mut h, second_id);
    let custom_name: tau_proto::EventName = "demo.startup_state".parse().expect("event name");
    let trailing_name: tau_proto::EventName = "demo.after_query".parse().expect("event name");

    h.handle_extension_message(
        first_id,
        TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ExtensionEvent(
                tau_proto::CustomEvent::try_new(
                    custom_name.clone(),
                    Some("s1".into()),
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
                Some("s1".into()),
                CborValue::Text("AFTER START REQUEST".to_owned()),
            )
            .expect("valid custom event"),
        )),
    )
    .expect("stage trailing event");

    assert!(!event_log_contains_source_event(&h, first_id, |event| {
        event.name() == custom_name
    }));
    assert!(!h.agents.keys().any(|cid| cid.as_str().contains("q-staged")));
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );

    h.handle_extension_message(
        second_id,
        TestMessage::Ready(tau_proto::Ready {
            message: Some("second ready first".to_owned()),
        }),
    )
    .expect("second ready");
    assert!(!event_log_contains_source_event(&h, second_id, |event| {
        matches!(event, Event::StartAgentRequest(_))
    }));
    h.handle_extension_message(
        first_id,
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
        let mut seq = crate::event_log::EventLogSeq::new(0);
        while let Some(entry) = h.event_log.get_next_from(seq) {
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
            (Some(first_id.into()), custom_name),
            (
                Some(second_id.into()),
                tau_proto::EventName::AGENT_START_REQUEST
            ),
            (Some(first_id.into()), trailing_name)
        ]
    );
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
    h.bus
        .set_subscriptions(
            "terminal-output-observer",
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
            conn_id,
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

    h.handle_extension_message(conn_id, TestMessage::Ready(Default::default()))
        .expect("activate terminal-output owner");

    let committed: Vec<_> = {
        let mut names = Vec::new();
        let mut seq = crate::event_log::EventLogSeq::new(0);
        while let Some(entry) = h.event_log.get_next_from(seq) {
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
                Some(tau_proto::ConnectionId::from(conn_id)),
                false,
                tau_proto::EventName::TERM_BELL,
            ),
            (
                Some(tau_proto::ConnectionId::from(conn_id)),
                false,
                tau_proto::EventName::TERM_OSC1337_SET_USER_VAR,
            ),
        ]
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn all_non_declaration_events_wait_for_the_global_activation_barrier() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.initial_extension_tool_preflight_complete = false;
    connect_handshaking_tool(&mut h, "operational-owner");
    connect_handshaking_tool(&mut h, "activation-blocker");
    h.pending_ui_shell_commands.insert(
        UiShellRouteId::new("startup-shell".into()),
        PendingUiShellCommand {
            provider_id: "operational-owner".into(),
            command: tau_proto::UiShellCommand {
                command_id: "startup-shell".into(),
                session_id: "s1".into(),
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
                command_id: "startup-shell".into(),
                session_id: "s1".into(),
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
    h.handle_extension_message("operational-owner", TestMessage::Ready(Default::default()))
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

    h.handle_extension_message("activation-blocker", TestMessage::Ready(Default::default()))
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

/// Disconnecting the last per-agent context waiter must resume a prompt that
/// already committed but was deferred before its model snapshot was frozen.
#[test]
fn context_provider_disconnect_resumes_publish_idle_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    h.selected_model = Some("test/model".into());
    let conn_id = "disconnecting-agent-context";
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
    h.handle_extension_message(conn_id, TestMessage::Ready(Default::default()))
        .expect("ready");

    h.dispatch_user_prompt("s1".into(), "resume after disconnect".to_owned())
        .expect("dispatch user prompt");
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );
    assert!(!h.pending_publish_idle_dispatches.is_empty());
    let agent_id = h
        .agents
        .values()
        .find_map(|agent| agent.agent_id.clone())
        .map(|agent_id| tau_proto::AgentId::parse(&agent_id).expect("agent id"))
        .expect("loaded agent");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ExtAgentContextPublish(
            tau_proto::ExtAgentContextPublish {
                agent_id: agent_id.clone(),
                key: "disconnect-test".into(),
                value: tau_proto::AgentContextValue(serde_json::json!("stale")),
            },
        )),
    )
    .expect("publish context before disconnect");
    assert!(
        h.agent_context
            .template_value(Some(&agent_id))
            .to_string()
            .contains("stale")
    );

    h.handle_disconnect(conn_id);

    assert!(h.pending_publish_idle_dispatches.is_empty());
    assert!(
        !h.agent_context
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
        TestProtocolItem::Event(Event::ToolRegistrationDeclared(
            tau_proto::ToolRegistrationDeclared {
                tool: staged_tool_spec("dropped_tool"),
                tool_group: None,
                prompt_fragment: Some(tau_proto::PromptFragment::new(
                    "dropped.tool.fragment",
                    tau_proto::PromptPriority::new(10),
                    "DROPPED TOOL FRAGMENT",
                )),
            },
        )),
    )
    .expect("stage tool");
    h.handle_extension_event(
        conn_id,
        TestProtocolItem::Event(Event::ProviderModelsDeclared(
            tau_proto::ProviderModelsDeclared {
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
            persist: true,
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

/// Staged captured-model presence followed by final absence is coalesced into
/// one authoritative removal without exposing the intermediate route.
#[test]
fn provider_ready_coalesces_staged_model_snapshots_to_final_state() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let mut captured_info = h
        .provider_model_info
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
    assert!(!h.provider_model_info.contains_key(&captured));
    assert!(
        h.enqueued_standalone_inference_checkpoints.is_empty(),
        "pre-Ready snapshots must not reconcile restored work"
    );

    h.handle_extension_message(
        conn_id,
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("activate provider");
    assert!(
        !h.provider_model_info.contains_key(&captured),
        "an intermediate staged route must never become the active provider route"
    );
    assert!(!h.provider_model_routes.contains_key(&captured));
    assert!(
        !h.enqueued_standalone_inference_checkpoints
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
        .provider_model_info
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
    h.provider_model_info.insert(current.clone(), current_info);
    h.provider_model_routes
        .insert(current.clone(), "other-provider".into());
    let role = h
        .available_roles
        .get_mut(&h.selected_role)
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
    h.agents.get_mut(&cid).expect("agent").model_override = Some(current);
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
    h.handle_extension_message(tool_conn_id, TestMessage::Ready(Default::default()))
        .expect("tool Ready waits on provider");
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentInferenceDispatchStarted(checkpoint)
            if checkpoint.transaction_id.as_ref() == Some(&transaction_id)
    )));

    h.handle_extension_message(
        conn_id,
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
    assert_eq!(prompt.session_id, h.current_session_id);
    assert_eq!(prompt.originator, tau_proto::PromptOriginator::User);
    assert_eq!(h.prompt_models.get(&checkpoint_prompt_id), Some(&captured));
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
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "before unregister");
    let before_spid = h.send_prompt_to_agent("s1");
    let before_prompt = read_prompt_created(&h, &before_spid);
    assert!(prompt_has_tool(&before_prompt, "shell"));
    h.handle_provider_response_finished(super::dispatch::provider_text_response(
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
        context_limit_telemetry: None,
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
fn duplicate_provider_is_rejected_without_ambiguous_fallback() {
    // A different connection cannot become a hidden fallback owner selected by
    // registration arrival order.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let spec = shell_tool_spec(&h);
    let report = h.registry.register("conn-duplicate-shell", spec);
    assert!(!report.errors.is_empty());
    let notice = tool_unavailable_notice_prompt(&ToolName::new("shell"));

    unregister_shell(&mut h);
    assert!(h.registry.providers_for("shell").is_empty());

    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::user("after partial unregister".to_owned()),
    )
    .expect("dispatch user prompt");

    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(context_text_count(&prompt, &notice), 1);
    assert_eq!(agent_prompt_text_count(&h, &notice), 1);
    assert!(!prompt_has_tool(&prompt, "shell"));

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
        assert!(h.registry.providers_for(name).is_empty());
    }
    let mut seq = crate::event_log::EventLogSeq::new(0);
    let mut saw_notice = false;
    while let Some(entry) = h.event_log.get_next_from(seq) {
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
            literal: false,
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
        context_limit_telemetry: None,
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
            context_limit_telemetry: None,
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
            allows_provider_image: false,
        },
    );
    h.pending_tool_providers
        .insert(call_id.clone(), conn_id.clone().into());
    h.tool_turn
        .record_unqueued_in_flight(cid.clone(), call_id.clone());
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
                ..
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
            literal: false,
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
        context_limit_telemetry: None,
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
        &h.tx,
    )
    .expect("spawn late tool");
    let conn_id = spawned.connection_id.clone();
    h.queue_extension_connect(ExtensionConnectCommand {
        entry: ExtensionEntry {
            tool_prefix: None,
            name: "late-tool".to_owned(),
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

    assert!(h.bus.connection(&conn_id).is_none());
    assert!(!h.extensions.entries.contains_key(&conn_id));

    let event =
        h.rx.recv_timeout(Duration::from_secs(1))
            .expect("connect command should be first");
    match event {
        HarnessEvent::Command(command) => h.handle_harness_command(command).expect("handle"),
        HarnessEvent::FromConnection { .. }
        | HarnessEvent::Disconnected { .. }
        | HarnessEvent::ReadFailed { .. }
        | HarnessEvent::NewClient(_)
        | HarnessEvent::SupervisedWriterCleanupComplete { .. } => {
            panic!("reader forwarded before connect command")
        }
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
            ..
        } => {
            assert_eq!(connection_id, conn_id);
            assert!(matches!(message.as_ref(), HarnessInputMessage::Hello(_)));
        }
        HarnessEvent::Command(_)
        | HarnessEvent::Disconnected { .. }
        | HarnessEvent::ReadFailed { .. }
        | HarnessEvent::NewClient(_)
        | HarnessEvent::SupervisedWriterCleanupComplete { .. } => {
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

    h.selected_model = Some("test/model".into());
    h.selected_role = "engineer".to_owned();
    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_agents.insert("sp-x".into(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
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
        context_limit_telemetry: None,
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

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("route failure".to_owned()))
        .expect("dispatch checkpointed prompt");
    let agent_prompt_id = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentPromptStarted(started) if started.model == "test/model".into() => {
                Some(started.agent_prompt_id)
            }
            _ => None,
        })
        .expect("compact prompt fact committed before route failure");

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
            literal: false,
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
        context_limit_telemetry: None,
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
            literal: false,
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
        context_limit_telemetry: None,
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
        context_limit_telemetry: None,
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
        context_limit_telemetry: None,
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
        context_limit_telemetry: None,
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
        context_limit_telemetry: None,
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
    assert!(disable_notices[0].always_show);
    assert_eq!(disable_notices[0].level, tau_proto::NoticeLevel::Warning);
    assert!(disable_notices[0].message.len() <= MAX_EXTENSION_RESTART_NOTICE_BYTES);
    h.shutdown().expect("shutdown");
}

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
        .insert("budget-disabled".into());
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
    h.schedule_extension_restart_at(connection_id, scheduled_at);
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
    h.bus
        .send_to(
            &connection_id,
            None,
            HarnessOutputMessage::deliver(Event::HarnessNotice(tau_proto::HarnessNotice {
                kind: tau_proto::notice_kind::HARNESS_NOTICE.to_owned(),
                message: "x".repeat(2 * 1024 * 1024),
                level: tau_proto::NoticeLevel::Info,
                always_show: false,
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
        match h
            .rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cleanup-complete event")
        {
            HarnessEvent::SupervisedWriterCleanupComplete {
                connection_id: completed,
            } if completed == connection_id => {
                h.handle_supervised_writer_cleanup_complete_at(&completed, cleanup_at)
                    .expect("join cleaned writer");
                break;
            }
            HarnessEvent::Disconnected { .. } => {}
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
        h.bus
            .send_to(
                &connection_id,
                None,
                HarnessOutputMessage::deliver(Event::HarnessNotice(tau_proto::HarnessNotice {
                    kind: tau_proto::notice_kind::HARNESS_NOTICE.to_owned(),
                    message: "x".repeat(2 * 1024 * 1024),
                    level: tau_proto::NoticeLevel::Info,
                    always_show: false,
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
    let expected = children
        .into_iter()
        .map(|(connection_id, _)| connection_id)
        .collect::<HashSet<_>>();
    let mut readers_finished = HashSet::new();
    let reader_deadline = Instant::now() + Duration::from_secs(2);
    while readers_finished.len() < expected.len() && Instant::now() < reader_deadline {
        let remaining = reader_deadline.saturating_duration_since(Instant::now());
        let Ok(event) = h.rx.recv_timeout(remaining) else {
            break;
        };
        if let HarnessEvent::Disconnected { connection_id } = event
            && expected.contains(&connection_id)
        {
            readers_finished.insert(connection_id);
        }
    }
    assert_eq!(readers_finished, expected);
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

#[test]
fn hello_protocol_version_mismatch_is_rejected() {
    let hello = tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION + 1,
        client_name: "future-client".into(),
        client_kind: tau_proto::ClientKind::Tool,
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
        "bridge",
        TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "bridge".into(),
            client_kind: tau_proto::ClientKind::Tool,
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
            context_limit_telemetry: None,
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
            allows_provider_image: false,
        },
    );
    h.pending_tool_providers
        .insert(call_id.clone(), "owner-ext".into());
    let completed_before = h.completed_tool_calls.clone();
    let (in_flight_before, total_before) = {
        let agent = &h.agents[&cid];
        (agent.tools_in_flight, agent.tools_total)
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
    assert_eq!(h.completed_tool_calls, completed_before);
    assert_eq!(h.agents[&cid].tools_in_flight, in_flight_before);
    assert_eq!(h.agents[&cid].tools_total, total_before);
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
            context_limit_telemetry: None,
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
        context_limit_telemetry: None,
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
        context_limit_telemetry: None,
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

/// A pending non-tool side query must terminalize its parent tool call while
/// retaining committed message activation for the side agent.
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
    }
    seed_agent_thinking(&mut h, &cid, "sp-query-pending");
    h.prompt_agents
        .insert("sp-query-pending".into(), cid.clone());
    h.publish_event(
        Some(HARNESS_CONNECTION_ID),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: "query-pending-message".into(),
            sender_id: tau_proto::AgentId::parse("manager").expect("sender id"),
            sender_session_id: None,
            recipient_id: durable_agent_id.clone(),
            kind: tau_proto::AgentMessageKind::Message,
            watch_turn_state: None,
            watch_provider_status: None,
            message: "notice".to_owned(),
        }),
    );

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
        context_limit_telemetry: None,
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
        context_limit_telemetry: None,
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

    h.apply_extension_prompt_fragment(
        "ctx-ext",
        tau_proto::ExtPromptFragmentPublish {
            fragment: tau_proto::PromptFragment::new(
                "ctx-fragment",
                tau_proto::PromptPriority::new(100),
                "stale fragment",
            ),
        },
    );
    h.apply_agent_context_publish(
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

    h.apply_extension_prompt_fragment(
        "ctx-ext",
        tau_proto::ExtPromptFragmentPublish {
            fragment: tau_proto::PromptFragment::new(
                "ctx-fragment",
                tau_proto::PromptPriority::new(100),
                "old session fragment",
            ),
        },
    );
    h.apply_agent_context_publish(
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
