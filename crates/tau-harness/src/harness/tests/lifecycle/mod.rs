use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{File, Permissions};
use std::io as path_std_io;
use std::io::{ErrorKind, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tau_config::secret_sources::SecretSources;
use tau_config::settings::{
    BuiltinComponentIdentity, HarnessSettings, TauRuntimeSocketAccess, TauStateAccess,
};

use super::*;
use crate::agent::{Agent, OuterTurnRuntimeState, OutputLengthContinuationState, PendingPrompt};
use crate::agent_creator_topology::RecordCreatorOutcome;
use crate::event::SUPERVISED_CLEANUP_GRACE;
use crate::event_log as path_crate_event_log;
use crate::extension::{
    ExtensionConnectCommand, ExtensionEntry, ExtensionState, spawn_in_process, spawn_supervised,
};
use crate::harness::extensions::StartupDeadline;
use crate::harness::provider_startup::{self, ProviderStartupSnapshot};
use crate::harness::{
    ClientWriterFailure, EXTENSION_RESTART_DELAY, HarnessSessionLaunch, HarnessStartupInputs,
    MAX_EXTENSION_RESTART_ATTEMPTS, MAX_EXTENSION_RESTART_NOTICE_BYTES, PendingTool,
    PendingUiShellCommand, UiShellRouteId, extension_disconnected_tool_call_error_message,
    extension_restart_disabled_notice, prompt_snapshot_tool_error_message,
    tool_available_again_notice_prompt, tool_unavailable_notice_prompt,
    unavailable_tool_error_message, validate_protocol_version,
};
use crate::settings::{
    Config, ExtensionConfig, ExtensionStartupDiagnosticKind, TauStateAccessSource,
};

static STUCK_PROVIDER_OBSERVED_TRANSPORT_CLOSE: AtomicBool = AtomicBool::new(false);
static STUCK_PROVIDER_RELEASE: AtomicBool = AtomicBool::new(false);
static STUCK_PROVIDER_EXITED: AtomicBool = AtomicBool::new(false);
static INVALID_CONFIG_PROVIDER_STARTS: AtomicUsize = AtomicUsize::new(0);

#[cfg(unix)]
const RAW_SECRET_STARTUP_TEST: &str = "harness::tests::lifecycle::provider_lifecycle::raw_secret_source_error_prevents_provider_start";

/// Releases a deliberately stuck provider if its shutdown test exits early.
struct StuckProviderRelease {
    /// Cancels the outer watchdog after normal bounded shutdown.
    watchdog_cancel: mpsc::Sender<()>,
    /// Watchdog that prevents a regressed unbounded join from hanging the
    /// suite.
    watchdog: Option<std::thread::JoinHandle<()>>,
}

impl Drop for StuckProviderRelease {
    fn drop(&mut self) {
        STUCK_PROVIDER_RELEASE.store(true, Ordering::SeqCst);
        let _ = self.watchdog_cancel.send(());
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while !STUCK_PROVIDER_EXITED.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn test_session_id(value: impl Into<String>) -> tau_proto::SessionId {
    tau_proto::SessionId::parse(value).expect("test session id")
}

fn test_agent_prompt_id(value: impl Into<String>) -> tau_proto::AgentPromptId {
    tau_proto::AgentPromptId::parse(value).expect("test agent prompt id")
}

fn context_text(item: &ContextItem) -> Option<&str> {
    match item {
        ContextItem::Message(message) => message.content.first().map(|part| match part {
            ContentPart::Text { text }
            | ContentPart::SyntheticCompactionSummary { text }
            | ContentPart::HarnessInternalText { text } => text.as_str(),
            ContentPart::UrlCitation { .. } | ContentPart::CitationMetadataInvalid => "",
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
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
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
        component: None,
        require: true,
        startup_timeout: Duration::from_secs(2),
        cwd: None,
        config: serde_json::json!({}),
        secrets: BTreeMap::new(),
        tau_state_access: TauStateAccess::Legacy,
        tau_runtime_socket_access: TauRuntimeSocketAccess::Hidden,
    }
}

fn connect_supervised_test_process(
    h: &mut Harness,
    config: ExtensionConfig,
    kind: tau_proto::ClientKind,
) -> (tau_proto::ConnectionId, u32) {
    let spawned = spawn_supervised(
        &config,
        kind.clone(),
        None,
        &h.runtime_io.tx,
        &h.runtime_io.component_ingress_tx,
        &h.session_runtime.state_dir,
        h.session_runtime.storage_mode.is_memory_only(),
        &Default::default(),
        None,
        0,
    )
    .expect("spawn supervised test process");
    let connection_id = spawned.connection_id.clone();
    let child_pid = spawned.child_pid;
    h.connect_extension(ExtensionConnectCommand {
        entry: ExtensionEntry {
            tool_prefix: config.tool_prefix.clone(),
            name: crate::test_extension_name(config.name.clone()),
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

        let event = h.expand_component_ingress_wake(
            h.runtime_io
                .rx
                .recv_timeout(Duration::from_secs(1))
                .expect("extension lifecycle event"),
        );
        match event {
            HarnessEvent::FromConnection {
                connection_id,
                message,
                frame_bytes: _,
                decoded_at: _,
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
            HarnessEvent::ComponentIngressReady => unreachable!("wake expanded"),
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
    h.tool_routing
        .registry
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
        hosted_tool_capabilities: Vec::new(),
        supported_tool_types: vec![tau_proto::ToolType::Function],
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: true,
        default_affinity: 100,
        context_window: tau_proto::TokenCount::new(4_096),
        max_input_tokens: None,
        max_output_tokens: None,
        efforts: tau_proto::ReasoningEffortCapability::mapped(vec![
            tau_proto::NativeReasoningEffort::Medium,
        ]),
        verbosities: vec![tau_proto::Verbosity::Medium],
        thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_generation_negative: false,
        standalone_compaction_threshold: None,
        standalone_compaction_prefix_budget: None,
        cache_policy: None,
        est_uncached_input_cost_1m_usd: Default::default(),
        est_cached_input_cost_1m_usd: Default::default(),
        est_cache_write_input_cost_1m_usd: Default::default(),
        est_output_cost_1m_usd: Default::default(),
        est_cache_storage_cost_1m_token_hour_usd: None,
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
    let agent_id = crate::parse_agent_id(
        h.agent_runtime.agent_registry.agents[cid]
            .identity
            .agent_id
            .as_deref()
            .expect("durable agent"),
    );
    let transaction_id =
        tau_proto::CompactionTransactionId::parse(transaction).expect("transaction id");
    let compact_prompt_id = test_agent_prompt_id(format!("ap-{transaction}-compact"));
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
            original_input_tokens: None,
            compaction_output_tokens: None,
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
        h.session_runtime
            .agent_store
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
        .session_runtime
        .agent_store
        .agent(agent_id.as_str())
        .and_then(tau_core::AgentTree::standalone_compaction_recovery)
        .expect("core recovery projection");
    let tau_core::StandaloneCompactionRecovery::AwaitingCheckpoint { through, .. } = &recovery
    else {
        panic!("successful resumable compaction awaits its checkpoint");
    };
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(cid)
        .expect("runtime agent")
        .identity
        .head = through.as_option();
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
    let connection_id: tau_proto::ConnectionId = crate::test_connection_id(conn_id);
    h.extensions.entries.insert(
        connection_id.clone(),
        ExtensionEntry {
            tool_prefix: None,
            name: crate::test_extension_name(conn_id),
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

/// Installs a configured Tool connection in the handshaking state and returns
/// its routed-frame sink for tests that exercise pre-Ready activation.
pub(super) fn connect_handshaking_tool(
    h: &mut Harness,
    conn_id: &str,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    connect_handshaking_extension(h, conn_id, tau_proto::ClientKind::Tool)
}

fn configure_supervised_extension(
    h: &mut Harness,
    connection_id: &str,
    kind: tau_proto::ClientKind,
) -> tau_proto::Configure {
    let sink = connect_handshaking_extension(h, connection_id, kind.clone());
    let entry = h
        .extensions
        .entries
        .get_mut(connection_id)
        .expect("extension");
    entry.supervised_config = Some(crate::settings::ExtensionConfig {
        tool_prefix: None,
        name: connection_id.to_owned(),
        command: "tau-test-extension".to_owned(),
        args: vec!["--stdio".to_owned()],
        role: (kind == tau_proto::ClientKind::Provider).then(|| "provider".to_owned()),
        component: None,
        require: true,
        startup_timeout: Duration::from_secs(2),
        cwd: None,
        config: serde_json::json!({}),
        secrets: BTreeMap::new(),
        tau_state_access: TauStateAccess::Legacy,
        tau_runtime_socket_access: TauRuntimeSocketAccess::Hidden,
    });
    entry.state = ExtensionState::Spawning;

    h.handle_extension_event(
        connection_id,
        TestProtocolItem::Message(TestMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("tau-test-extension"),
            client_kind: kind,
            expected_session_id: None,
            capabilities: Default::default(),
        })),
    )
    .expect("hello");

    sink.lock()
        .expect("sink")
        .iter()
        .find_map(|routed| match &routed.frame {
            HarnessOutputMessage::Configure(configure) => Some(configure.clone()),
            _ => None,
        })
        .expect("configure sent")
}

fn builtin_provider_startup_config(
    source_declaration: Option<tau_config::settings::ExtensionSecretEntry>,
) -> crate::settings::Config {
    crate::settings::Config {
        extensions: BTreeMap::from([(
            "provider-work".to_owned(),
            crate::settings::ExtensionConfig {
                name: "provider-work".to_owned(),
                command: "tau".to_owned(),
                args: vec!["component".to_owned(), "ext-provider-builtin".to_owned()],
                role: Some("provider".to_owned()),
                component: Some(BuiltinComponentIdentity::Provider),
                tool_prefix: None,
                require: true,
                startup_timeout: Duration::from_secs(2),
                cwd: None,
                config: serde_json::json!({}),
                secrets: source_declaration
                    .map(|declaration| BTreeMap::from([("provider_key".to_owned(), declaration)]))
                    .unwrap_or_default(),
                tau_state_access: TauStateAccess::Legacy,
                tau_runtime_socket_access: TauRuntimeSocketAccess::Hidden,
            },
        )]),
        extension_startup_diagnostics: Vec::new(),
        harness_settings: HarnessSettings::built_in(),
    }
}

fn named_provider_settings() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "kind": "chat_completions",
        "credential": {
            "kind": "api_key",
            "identity": "0123456789abcdef0123456789abcdef",
            "source": {"kind": "named_secret", "name": "provider_key"}
        }
    }))
    .expect("settings")
}

fn insert_extension_entry_with_meter(
    h: &mut Harness,
    connection_id: &str,
    name: &str,
    state: ExtensionState,
    protocol_io: tau_client::ProtocolIoMeter,
) {
    let connection_id: tau_proto::ConnectionId = crate::test_connection_id(connection_id);
    h.extensions.entries.insert(
        connection_id.clone(),
        ExtensionEntry {
            tool_prefix: None,
            name: crate::test_extension_name(name),
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

pub(super) fn connect_socket_ui(
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

pub(super) fn read_notice<R: std::io::Read>(
    reader: &mut HarnessOutputReader<R>,
) -> tau_proto::HarnessNotice {
    let message = reader
        .read_message()
        .expect("read notice")
        .expect("notice frame");
    let Some(Event::HarnessNotice(notice)) = peel_inner_event(&message) else {
        panic!("expected harness notice, got {message:?}");
    };
    notice.clone()
}

pub(super) fn assert_no_message<R: std::io::Read>(reader: &mut HarnessOutputReader<R>) {
    match reader.read_message() {
        Err(tau_proto::DecodeError::Io(error)) if error.kind() == ErrorKind::WouldBlock => {}
        Ok(None) => {}
        other => panic!("unexpected routed frame: {other:?}"),
    }
}

fn debug_event_stats_request(extension_name: &str) -> HarnessInputMessage {
    HarnessInputMessage::UiDebugEventStatsRequest(tau_proto::UiDebugEventStatsRequest {
        extension_name: crate::test_extension_name(extension_name),
    })
}

fn shutdown_request() -> HarnessInputMessage {
    HarnessInputMessage::UiShutdownRequest(tau_proto::UiShutdownRequest {})
}

fn tree_request(session_id: &str, target_agent_id: Option<&str>) -> HarnessInputMessage {
    HarnessInputMessage::UiTreeRequest(tau_proto::UiTreeRequest {
        session_id: test_session_id(session_id),
        target_agent_id: target_agent_id.map(crate::parse_agent_id),
    })
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
        presentation: Default::default(),
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

/// Writer that exposes completed protocol writes and flushes to lifecycle
/// tests.
struct RecordingWriter {
    /// Bytes accepted by the writer.
    committed: Arc<Mutex<Vec<u8>>>,
    /// Number of completed flush calls.
    flushes: usize,
    /// Shared projection of `flushes`.
    completed_flushes: Arc<AtomicUsize>,
}

/// Writer that holds one output write until a lifecycle test releases it.
struct StalledWriter {
    /// One-shot notification that the writer reached its blocking write.
    started: Option<SyncSender<()>>,
    /// Release signal for the blocked write.
    release: Receiver<()>,
}

impl path_std_io::Write for StalledWriter {
    fn write(&mut self, buf: &[u8]) -> path_std_io::Result<usize> {
        if let Some(started) = self.started.take() {
            started.send(()).expect("report stalled write");
        }
        self.release.recv().expect("release stalled write");
        Ok(buf.len())
    }

    fn flush(&mut self) -> path_std_io::Result<()> {
        Ok(())
    }
}

/// Returns the validated encoded size carried beside a synthetic input frame.
fn lifecycle_input_frame_bytes(message: &HarnessInputMessage) -> tau_proto::ProtocolMessageBytes {
    tau_proto::ProtocolMessageBytes::new(
        tau_proto::encode_message_to_vec(message)
            .expect("encode lifecycle input")
            .len() as u64,
    )
    .expect("lifecycle input frame is nonempty")
}

impl path_std_io::Write for RecordingWriter {
    fn write(&mut self, buf: &[u8]) -> path_std_io::Result<usize> {
        self.committed
            .lock()
            .expect("committed output")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> path_std_io::Result<()> {
        self.flushes += 1;
        self.completed_flushes
            .store(self.flushes, Ordering::Release);
        Ok(())
    }
}

/// Construct one eligible reasoning-only output-cap response.
fn reasoning_only_length_response(
    prompt: &tau_proto::AgentPromptCreated,
    response_received_tokens: u64,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: prompt.agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        output_items: vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text: "retained reasoning".to_owned(),
        })],
        stop_reason: tau_proto::ProviderStopReason::Length,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        provider_attempt: Default::default(),
        usage: Some(tau_proto::ProviderTokenUsage {
            response_received_tokens,
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: Some(tau_proto::ProviderBackend {
            kind: tau_proto::ProviderBackendKind::ChatCompletions,
            base_url: "https://example.invalid/v1".to_owned(),
            transport: tau_proto::ProviderBackendTransport::HttpSse,
            stale_chain_fallback: false,
        }),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

/// Builds a validated shell command id used by this test module.
fn test_shell_command_id(value: impl AsRef<str>) -> tau_proto::ShellCommandId {
    tau_proto::ShellCommandId::parse(value.as_ref())
        .expect("test identifier must satisfy its grammar")
}

mod agent_lifecycle;
mod client_requests;
mod compaction_lifecycle;
mod cost_accounting;
mod debug_observability;
mod extension_lifecycle;
mod provider_lifecycle;
mod session_lifecycle;
