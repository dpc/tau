//! Unit coverage implementing `testing.md`.

use std::io as path_std_io;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Condvar, Mutex, mpsc};
use std::time::Duration;

use tau_proto::{HarnessInputMessage, HarnessOutputMessage, ToolStarted};
use xmpp_parsers::stanza_error as path_xmpp_parsers_stanza_error;

use super::*;

static SATURATION_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Clears the correlated production saturation hook after each test.
struct SaturationHookGuard;

impl Drop for SaturationHookGuard {
    fn drop(&mut self) {
        SATURATION_HOOK.lock().expect("xmpp saturation hook").take();
    }
}

/// Cloneable tracing sink for warning-content assertions.
#[derive(Clone, Default)]
struct TraceWriter(Arc<Mutex<Vec<u8>>>);

impl Write for TraceWriter {
    fn write(&mut self, bytes: &[u8]) -> path_std_io::Result<usize> {
        self.0.lock().expect("trace bytes").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> path_std_io::Result<()> {
        Ok(())
    }
}

/// Production writer blocked by the first detached saturation filler.
struct SaturationWriter {
    /// Serialized protocol output.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Writer gate, initially closed.
    gate: Arc<(Mutex<bool>, Condvar)>,
    /// Notification that the writer reached the filler.
    entered: mpsc::Sender<()>,
    /// Whether this writer already blocked once.
    blocked: bool,
}

impl Write for SaturationWriter {
    fn write(&mut self, bytes: &[u8]) -> path_std_io::Result<usize> {
        if !self.blocked && bytes.windows(9).any(|window| window == b"term.bell") {
            self.blocked = true;
            let _ = self.entered.send(());
            let (lock, wake) = &*self.gate;
            let closed = lock.lock().expect("writer gate");
            drop(
                wake.wait_while(closed, |closed| *closed)
                    .expect("wait for writer release"),
            );
        }
        self.bytes
            .lock()
            .expect("output bytes")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> path_std_io::Result<()> {
        Ok(())
    }
}

/// Production writer that fails when one selected mandatory frame is written.
struct FailingWriter {
    /// Complete bytes written before failure.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Event name whose frame must fail.
    target: &'static [u8],
    /// Optional notification that the selected frame reached the writer.
    failed: Option<mpsc::Sender<()>>,
}

impl Write for FailingWriter {
    fn write(&mut self, bytes: &[u8]) -> path_std_io::Result<usize> {
        if bytes
            .windows(self.target.len())
            .any(|window| window == self.target)
        {
            if let Some(failed) = self.failed.take() {
                let _ = failed.send(());
            }
            return Err(path_std_io::Error::other("forced XMPP writer failure"));
        }
        self.bytes
            .lock()
            .expect("output bytes")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> path_std_io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct SharedWriter {
    /// Shared byte buffer written by the runner's writer thread.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    /// Returns a snapshot of bytes written so far.
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("lock shared writer").clone()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().expect("lock shared writer").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct FakeBridge {
    started: Mutex<usize>,
    shutdowns: Mutex<usize>,
    ready: Mutex<bool>,
    ready_changed: Condvar,
    readiness_error: Mutex<Option<String>>,
    wait_timeouts: Mutex<Vec<Duration>>,
    wait_recorded: Condvar,
    registered: Mutex<HashMap<AgentId, String>>,
    /// Exact generation currently installed in the fake remote bridge.
    remote_leases: Mutex<HashMap<AgentId, RegistrationLease>>,
    /// Every completed remote registration, including later-cleaned
    /// generations.
    registrations: Mutex<Vec<(AgentId, String)>>,
    /// Optional enqueue failure for best-effort unregister cleanup.
    unregister_error: Mutex<Option<String>>,
    /// Exact generation passed to each best-effort cleanup enqueue.
    cleanup_leases: Mutex<Vec<(AgentId, RegistrationLease)>>,
    /// Whether the next registration must stop at the deterministic test
    /// barrier.
    block_next_register: Mutex<bool>,
    /// Signals that the blocked registration reached its barrier.
    register_entered: Condvar,
    /// Whether the blocked registration may complete.
    register_released: Mutex<bool>,
    /// Releases the blocked registration without a timing sleep.
    register_release: Condvar,
    /// Shared authority captured at startup for cleanup-order assertions.
    authority: Mutex<Option<Arc<RegistrationAuthority>>>,
    sent: Mutex<Vec<(AgentId, String)>>,
    send_error: Mutex<Option<(usize, String)>>,
    /// Optional worker-origin report released by a deterministic test gate.
    worker_report: Mutex<Option<(mpsc::Receiver<()>, Event)>>,
}

impl FakeBridge {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn set_ready(&self, ready: bool) {
        *self.ready.lock().expect("lock") = ready;
        self.ready_changed.notify_all();
    }

    fn set_readiness_error(&self, message: &str) {
        *self.readiness_error.lock().expect("lock") = Some(message.to_owned());
    }

    fn set_send_error(&self, message: &str) {
        self.set_send_error_after(0, message);
    }

    fn set_send_error_after(&self, successful_sends: usize, message: &str) {
        *self.send_error.lock().expect("lock") = Some((successful_sends, message.to_owned()));
    }

    fn wait_for_wait_calls(&self, count: usize) -> Vec<Duration> {
        let calls = self.wait_timeouts.lock().expect("lock");
        let (calls, result) = self
            .wait_recorded
            .wait_timeout_while(calls, Duration::from_secs(1), |calls| calls.len() < count)
            .expect("lock");
        assert!(
            !result.timed_out(),
            "timed out waiting for {count} readiness wait call(s)"
        );
        calls.clone()
    }

    /// Block exactly the next remote registration completion.
    fn block_next_registration(&self) {
        *self.block_next_register.lock().expect("lock") = true;
        *self.register_released.lock().expect("lock") = false;
    }

    /// Wait until the selected registration reaches its deterministic barrier.
    fn wait_for_blocked_registration(&self) {
        let blocked = self.block_next_register.lock().expect("lock");
        let (_blocked, result) = self
            .register_entered
            .wait_timeout_while(blocked, Duration::from_secs(1), |blocked| *blocked)
            .expect("lock");
        assert!(!result.timed_out(), "registration did not reach barrier");
    }

    /// Release the selected registration completion.
    fn release_registration(&self) {
        *self.register_released.lock().expect("lock") = true;
        self.register_release.notify_all();
    }
}

impl XmppBridge for FakeBridge {
    fn ensure_started(
        &self,
        _cfg: RuntimeConfig,
        output: Output,
        _shutdown: Arc<ShutdownSignal>,
        authority: Arc<RegistrationAuthority>,
    ) -> Result<(), String> {
        *self.authority.lock().expect("lock") = Some(authority);
        *self.started.lock().expect("lock") += 1;
        if let Some((release, event)) = self.worker_report.lock().expect("lock").take() {
            std::thread::spawn(move || {
                let _ = release.recv();
                let _ = output.emit_message_report(event);
            });
        }
        Ok(())
    }

    fn register_agent(
        &self,
        cfg: &RuntimeConfig,
        agent_id: &AgentId,
        lease: RegistrationLease,
        room_localpart: Option<&str>,
    ) -> Result<String, String> {
        {
            let mut block = self.block_next_register.lock().expect("lock");
            if *block {
                *block = false;
                self.register_entered.notify_all();
                drop(block);
                let released = self.register_released.lock().expect("lock");
                drop(
                    self.register_release
                        .wait_while(released, |released| !*released)
                        .expect("lock"),
                );
            }
        }
        let address = match cfg.routing_mode {
            RoutingMode::Muc => format!(
                "{}@conference.example.org",
                room_localpart.expect("MUC room localpart")
            ),
            RoutingMode::DirectResource => "tau@example.org/tau-test".to_owned(),
        };
        self.registered
            .lock()
            .expect("lock")
            .insert(agent_id.clone(), address.clone());
        self.remote_leases
            .lock()
            .expect("lock")
            .insert(agent_id.clone(), lease);
        self.registrations
            .lock()
            .expect("lock")
            .push((agent_id.clone(), address.clone()));
        Ok(address)
    }

    fn unregister_agent(&self, agent_id: &AgentId, lease: RegistrationLease) -> Result<(), String> {
        if let Some(authority) = self.authority.lock().expect("lock").as_ref() {
            assert_eq!(
                authority.publish_if_active(agent_id, lease, || ()),
                None,
                "cleanup was enqueued before exact local lease revocation"
            );
        }
        self.cleanup_leases
            .lock()
            .expect("lock")
            .push((agent_id.clone(), lease));
        if let Some(error) = self.unregister_error.lock().expect("lock").clone() {
            return Err(error);
        }
        let mut remote_leases = self.remote_leases.lock().expect("lock");
        if remote_leases.get(agent_id) == Some(&lease) {
            remote_leases.remove(agent_id);
            self.registered.lock().expect("lock").remove(agent_id);
        }
        Ok(())
    }

    fn wait_until_ready(&self, timeout: Duration) -> Result<(), String> {
        self.wait_timeouts.lock().expect("lock").push(timeout);
        self.wait_recorded.notify_all();
        if let Some(message) = self.readiness_error.lock().expect("lock").clone() {
            return Err(message);
        }
        let ready = self.ready.lock().expect("lock");
        let (ready, result) = self
            .ready_changed
            .wait_timeout_while(ready, timeout, |ready| !*ready)
            .expect("lock");
        if *ready {
            Ok(())
        } else {
            // ast-grep-ignore: debug-assert-expression-must-not-mutate
            debug_assert!(result.timed_out());
            Err(format!(
                "xmpp connection did not become online within {}s; retry after the account connects",
                timeout.as_secs()
            ))
        }
    }

    fn send_message(&self, agent_id: &AgentId, text: &str) -> Result<(), String> {
        let mut sent = self.sent.lock().expect("lock");
        if let Some((successful_sends, message)) = self.send_error.lock().expect("lock").clone()
            && sent.len() >= successful_sends
        {
            return Err(message);
        }
        sent.push((agent_id.clone(), text.to_owned()));
        Ok(())
    }

    fn shutdown(&self, _timeout: Duration) -> Result<(), String> {
        *self.shutdowns.lock().expect("lock") += 1;
        Ok(())
    }
}

fn agent_id(text: &str) -> AgentId {
    AgentId::parse(text).expect("agent id")
}

/// Stamp a bridge-produced delivered report payload as the harness would, then
/// prove its projection is identical after a serde round trip.
fn assert_delivered_live_replay_parity(report: MessageDelivered<tau_proto::RawMessagePublisherId>) {
    assert_eq!(report.publisher_extension_id.as_str(), "std-xmpp");
    let live = Event::MessageDeliveredReported(report)
        .into_stamped_canonical_message_fact(
            tau_proto::MessagePublisherId::parse("std-xmpp").expect("canonical publisher"),
        )
        .expect("delivered report converts to a canonical fact");
    let encoded = serde_json::to_value(&live).expect("encode fact");
    let replay: Event = serde_json::from_value(encoded).expect("decode replay fact");
    assert_eq!(
        tau_proto::project_message_fact(&live),
        tau_proto::project_message_fact(&replay)
    );
}

fn assert_room_shape(room: &BareJid, agent_id: &str) {
    let room = room.to_string();
    let expected_prefix = format!("{}-", agent_id.to_ascii_lowercase());
    assert!(room.starts_with(&expected_prefix), "{room}");
    assert!(room.ends_with("@conference.example.org"), "{room}");
    let localpart = room
        .split_once('@')
        .map(|(localpart, _)| localpart)
        .expect("room localpart");
    let disambiguator = localpart
        .strip_prefix(&expected_prefix)
        .expect("disambiguator");
    assert_eq!(disambiguator.len(), 8);
    assert!(
        disambiguator
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
    );
}

fn default_muc_room(worker: &WorkerState, agent_id: &AgentId) -> BareJid {
    let state = State::default();
    let localpart = room_localpart_for_registration(
        &state,
        &worker.cfg,
        &"session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        agent_id,
    )
    .expect("render default room")
    .expect("MUC localpart");
    worker.muc_room_for(&localpart).expect("room")
}

fn tool(name: &str, agent: &str, args: CborValue) -> ToolStarted {
    ToolStarted {
        call_id: format!("call-{name}").into(),
        tool_name: tau_proto::ToolName::new(name),
        arguments: args,
        agent_id: agent_id(agent),
        originator: tau_proto::PromptOriginator::User,
    }
}

fn bool_args(value: bool) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("enabled".to_owned()),
        CborValue::Bool(value),
    )])
}

fn bool_args_with_extra(value: bool) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("enabled".to_owned()),
            CborValue::Bool(value),
        ),
        (
            CborValue::Text("destination".to_owned()),
            CborValue::Text("mallory@example.org".to_owned()),
        ),
    ])
}

fn message_args(value: &str) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("message".to_owned()),
        CborValue::Text(value.to_owned()),
    )])
}

fn message_args_with_destination(value: &str) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("message".to_owned()),
            CborValue::Text(value.to_owned()),
        ),
        (
            CborValue::Text("destination".to_owned()),
            CborValue::Text("mallory@example.org".to_owned()),
        ),
    ])
}

fn cfg() -> RuntimeConfig {
    ExtConfig {
        jid: Some("tau@example.org".to_owned()),
        password_secret: Some("xmpp_password".to_owned()),
        allowed_jids: vec!["me@example.org".to_owned()],
        default_recipient: Some("me@example.org".to_owned()),
        routing: RoutingConfig {
            mode: Some("muc".to_owned()),
        },
        muc: MucConfigRaw {
            service: Some("conference.example.org".to_owned()),
            ..Default::default()
        },
        ..Default::default()
    }
    .validate(&secrets(), Some("std-xmpp".to_owned()))
    .expect("valid config")
}

fn secrets() -> BTreeMap<String, tau_proto::SecretValue> {
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "xmpp_password".to_owned(),
        tau_proto::SecretValue::new("secret"),
    );
    secrets
}

fn configure_from_json(config: serde_json::Value) -> tau_proto::Configure {
    tau_proto::Configure {
        tool_prefix: None,
        config: tau_proto::json_to_cbor(&config),
        instance_name: tau_proto::ExtensionName::parse("std-xmpp")
            .expect("test extension name must satisfy the identifier grammar"),
        state_dir: None,
        secrets: secrets(),
        settings_files: Default::default(),
    }
}

fn valid_config_message() -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(configure_from_json(serde_json::json!({
        "jid": "tau@example.org",
        "password_secret": "xmpp_password",
        "allowed_jids": ["me@example.org"],
        "default_recipient": "me@example.org",
        "routing": { "mode": "muc" },
        "muc": { "service": "conference.example.org" },
    })))
}

fn malformed_config_message() -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(configure_from_json(serde_json::json!({
        "unknown_field": true,
    })))
}

/// Two XMPP account instances use the generic SDK scope for tools, aliases, and
/// groups without changing semantic XMPP tags.
#[test]
fn generic_prefixes_scope_xmpp_instances() {
    for prefix in ["personal", "work"] {
        let HarnessOutputMessage::Configure(mut configure) = valid_config_message() else {
            unreachable!()
        };
        configure.tool_prefix = Some(tau_proto::ToolNamePrefix::parse(prefix).expect("prefix"));
        let frames = run_protocol_messages(
            &[
                HarnessOutputMessage::Configure(configure),
                HarnessOutputMessage::Disconnect(Default::default()),
            ],
            FakeBridge::new(),
        );
        let registrations = frames
            .iter()
            .filter_map(|frame| match frame {
                HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                    Event::ToolRegistrationDeclared(register) => Some(register),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(registrations.iter().any(|registration| {
            registration.tool.name.as_str() == format!("{prefix}_{REGISTER_TOOL_NAME}")
                && registration
                    .tool
                    .model_visible_name
                    .as_ref()
                    .is_some_and(|name| name.as_str() == format!("{prefix}_{REGISTER_TOOL_NAME}"))
                && registration.tool_group.as_ref().is_some_and(|group| {
                    group.name.as_str() == format!("{prefix}_{TOOL_GROUP_NAME}")
                })
        }));
        assert!(registrations.iter().any(|registration| {
            registration.tool.name.as_str() == format!("{prefix}_{SEND_TOOL_NAME}")
        }));
    }
}

fn session_started_message(session_id: &str) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver(Event::SessionStarted(tau_proto::SessionStarted {
        session_id: session_id
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        reason: tau_proto::SessionStartReason::Initial,
    }))
}

fn run_protocol_messages(
    messages: &[HarnessOutputMessage],
    bridge: Arc<FakeBridge>,
) -> Vec<HarnessInputMessage> {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    if !matches!(messages.first(), Some(HarnessOutputMessage::Configure(_))) {
        writer
            .write_message(&valid_config_message())
            .expect("write initial configure");
    }
    for message in messages {
        writer.write_message(message).expect("write input");
    }
    writer.flush().expect("flush input");

    let output = SharedWriter::default();
    let written = output.clone();
    run_with_bridge(path_std_io::Cursor::new(input), output, bridge).expect("run");

    let mut frames = Vec::new();
    let mut reader = tau_proto::HarnessInputReader::new(path_std_io::Cursor::new(written.bytes()));
    while let Some(frame) = reader.read_message().expect("read output") {
        frames.push(frame);
    }
    frames
}

/// A successful remote send must survive exhaustion of the production detached
/// FIFO and flush its mandatory sent report immediately before its sole
/// terminal.
#[test]
fn outbound_report_and_terminal_survive_production_fifo_saturation() {
    let _serial = SATURATION_TEST_LOCK
        .lock()
        .expect("xmpp saturation test lock");
    let mut input_bytes = Vec::new();
    let mut input = tau_proto::HarnessOutputWriter::new(&mut input_bytes);
    for message in [
        valid_config_message(),
        session_started_message("session-1"),
        HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))),
        HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            SEND_TOOL_NAME,
            "agent-1",
            message_args("saturated outbound"),
        ))),
        HarnessOutputMessage::Disconnect(Default::default()),
    ] {
        input.write_message(&message).expect("write input");
    }
    input.flush().expect("flush input");

    let bytes = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new((Mutex::new(true), Condvar::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (saturated_tx, saturated_rx) = mpsc::channel();
    *SATURATION_HOOK.lock().expect("xmpp saturation hook") =
        Some(("saturated outbound".to_owned(), saturated_tx));
    let hook = SaturationHookGuard;
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    let output_bytes = Arc::clone(&bytes);
    let output_gate = Arc::clone(&gate);
    let runner = std::thread::spawn(move || {
        run_with_bridge(
            path_std_io::Cursor::new(input_bytes),
            SaturationWriter {
                bytes: output_bytes,
                gate: output_gate,
                entered: entered_tx,
                blocked: false,
            },
            bridge,
        )
        .map_err(|error| error.to_string())
    });
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("production writer blocked");
    saturated_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("detached FIFO exhausted");
    drop(hook);
    let (closed, wake) = &*gate;
    *closed.lock().expect("writer gate") = false;
    wake.notify_all();
    runner.join().expect("runner").expect("clean disconnect");

    let mut reader = tau_proto::HarnessInputReader::new(path_std_io::Cursor::new(
        bytes.lock().expect("bytes").clone(),
    ));
    let events = std::iter::from_fn(|| reader.read_message().transpose())
        .collect::<Result<Vec<_>, _>>()
        .expect("decode output")
        .into_iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => Some(*emit.event),
            _ => None,
        })
        .collect::<Vec<_>>();
    let report_indices = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(event, Event::MessageSentReported(report) if report.text == "saturated outbound")
                .then_some(index)
        })
        .collect::<Vec<_>>();
    let terminal_indices = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(
                event,
                Event::ToolResultReported(result)
                    if result.call_id.as_str() == format!("call-{SEND_TOOL_NAME}")
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(report_indices.len(), 1);
    assert_eq!(terminal_indices, vec![report_indices[0] + 1]);
}

/// Failure of the checked outbound sent report must exit the production loop
/// and suppress its paired tool terminal.
#[test]
fn outbound_report_writer_failure_exits_without_paired_terminal() {
    let mut input_bytes = Vec::new();
    let mut input = tau_proto::HarnessOutputWriter::new(&mut input_bytes);
    let mut send = tool(
        SEND_TOOL_NAME,
        "agent-1",
        message_args("forced XMPP report failure"),
    );
    send.call_id = "gyf8-xmpp-failed-send".into();
    for message in [
        valid_config_message(),
        session_started_message("session-1"),
        HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))),
        HarnessOutputMessage::deliver(Event::ToolStarted(send)),
    ] {
        input.write_message(&message).expect("write input");
    }
    input.flush().expect("flush input");
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    let (failed_tx, failed_rx) = mpsc::channel();
    let result = run_with_bridge(
        path_std_io::Cursor::new(input_bytes),
        FailingWriter {
            bytes: Arc::clone(&bytes),
            target: b"message.sent_reported",
            failed: Some(failed_tx),
        },
        bridge,
    );
    assert!(result.is_err(), "sent-report failure must exit the loop");
    failed_rx
        .try_recv()
        .expect("selected writer failure occurred");

    let mut reader = tau_proto::HarnessInputReader::new(path_std_io::Cursor::new(
        bytes.lock().expect("bytes").clone(),
    ));
    while let Ok(Some(frame)) = reader.read_message() {
        assert!(
            !matches!(
                frame,
                HarnessInputMessage::Emit(emit)
                    if matches!(emit.event.as_ref(),
                        Event::ToolResultReported(result)
                            if result.call_id.as_str() == "gyf8-xmpp-failed-send"
                    ) || matches!(emit.event.as_ref(),
                        Event::ToolErrorReported(error)
                            if error.call_id.as_str() == "gyf8-xmpp-failed-send"
                    )
            ),
            "sent-report failure published its paired terminal"
        );
    }
}

/// A worker-origin report failure must wake an otherwise idle protocol loop and
/// run bridge cleanup.
#[test]
fn ingress_report_writer_failure_wakes_idle_production_loop() {
    let (extension_input, harness_input) = UnixStream::pair().expect("input pair");
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    let (release_tx, release_rx) = mpsc::channel();
    let report = Event::MessageDeliveredReported(MessageDelivered::new(
        RawMessagePublisherId::new("test-extension"),
        MessageAgentTarget::new("agent-1"),
        MessageFactId::new("xmpp-message:worker-failure"),
        MessageParty {
            stable_id: "xmpp-sender:worker".to_owned(),
            display_name: None,
            sender_auth: Some(MessageSenderAuth::VerifiedAllowlisted),
        },
        Some(MessageConversation {
            stable_id: "xmpp-conversation:worker".to_owned(),
            display_name: None,
            alias: None,
        }),
        "wake idle loop",
    ));
    *bridge.worker_report.lock().expect("worker report lock") = Some((release_rx, report));
    let runner_bridge: Arc<dyn XmppBridge> = bridge.clone();
    let (failed_tx, failed_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run_with_bridge(
            extension_input,
            FailingWriter {
                bytes: Arc::new(Mutex::new(Vec::new())),
                target: b"message.delivered_reported",
                failed: Some(failed_tx),
            },
            runner_bridge,
        )
        .map_err(|error| error.to_string());
        let _ = result_tx.send(result);
    });

    let mut input = tau_proto::HarnessOutputWriter::new(harness_input);
    for message in [
        valid_config_message(),
        session_started_message("session-1"),
        HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))),
    ] {
        input.write_message(&message).expect("write input");
    }
    input.flush().expect("flush startup");
    release_tx.send(()).expect("release worker report");

    failed_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("worker report reached failing writer");
    assert!(
        result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("idle loop exited")
            .is_err(),
        "worker output failure must fail the production runner"
    );
    assert_eq!(*bridge.shutdowns.lock().expect("shutdown count"), 1);
    assert_eq!(
        bridge.cleanup_leases.lock().expect("cleanup leases").len(),
        1,
        "output loss must retire the active registration before shutdown"
    );
}

fn empty_password_secrets() -> BTreeMap<String, tau_proto::SecretValue> {
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "xmpp_password".to_owned(),
        tau_proto::SecretValue::new("   "),
    );
    secrets
}

fn shutdown_signal() -> Arc<ShutdownSignal> {
    Arc::new(ShutdownSignal::new())
}

/// Returns isolated registration authority for state-only worker tests.
fn test_authority() -> Arc<RegistrationAuthority> {
    Arc::new(RegistrationAuthority::default())
}

/// Installs one exact active lease in both worker and shared authority state.
fn activate_worker_agent(worker: &mut WorkerState, agent_id: AgentId) -> RegistrationLease {
    let lease = worker.authority.reserve(agent_id.clone());
    assert!(worker.authority.activate(&agent_id, lease));
    worker.registration_leases.insert(agent_id, lease);
    lease
}

fn extension() -> (
    Extension,
    mpsc::Receiver<HarnessInputMessage>,
    Arc<FakeBridge>,
) {
    let (tx, rx) = mpsc::channel();
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    let ext = Extension::new(bridge.clone(), tx);
    ext.apply_config(cfg()).expect("apply config");
    ext.state.lock().expect("lock").current_session_id = Some(
        "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
    );
    (ext, rx, bridge)
}

/// XMPP bridge tools are disabled by default because exposing external chat to
/// a model must be an explicit role-policy choice.
#[test]
fn xmpp_tools_are_role_opt_in() {
    assert!(!register_tool_spec().enabled_by_default);
    assert!(!send_tool_spec().enabled_by_default);
}

/// XMPP bridge tools expose group and tag metadata so role policy can enable
/// registration and sending separately.
#[test]
fn xmpp_tools_have_group_and_tags() {
    assert_eq!(xmpp_tool_group().name.as_str(), TOOL_GROUP_NAME);
    assert!(
        register_tool_spec()
            .tags
            .iter()
            .any(|tag| tag.as_str() == REGISTER_TOOL_TAG)
    );
    assert!(
        send_tool_spec()
            .tags
            .iter()
            .any(|tag| tag.as_str() == SEND_TOOL_TAG)
    );
}

/// Provider-owned repair examples must stay schema-valid as bridge tool
/// argument shapes evolve.
#[test]
fn xmpp_tool_examples_are_schema_valid() {
    for spec in [register_tool_spec(), send_tool_spec()] {
        tau_core::validate_tool_examples(&spec)
            .unwrap_or_else(|error| panic!("invalid examples for {}: {error}", spec.name));
    }
}

/// Config validation fails closed when credentials, allowlist, routing, default
/// recipient, MUC service, or message-size limits are absent or unsafe.
#[test]
fn config_rejects_unsafe_shapes() {
    let err = ExtConfig::default()
        .validate(&BTreeMap::new(), None)
        .err()
        .expect("missing jid");
    assert!(err.contains("jid"));

    let err = ExtConfig {
        jid: Some("tau@example.org/resource".to_owned()),
        ..Default::default()
    }
    .validate(&BTreeMap::new(), None)
    .err()
    .expect("full jid rejected");
    assert!(err.contains("bare account JID"));

    let err = ExtConfig {
        jid: Some("tau@example.org".to_owned()),
        password_secret: Some("xmpp_password".to_owned()),
        ..Default::default()
    }
    .validate(&BTreeMap::new(), None)
    .err()
    .expect("missing password secret rejected");
    assert!(err.contains("missing or empty"));

    let err = ExtConfig {
        jid: Some("tau@example.org".to_owned()),
        password_secret: Some("xmpp_password".to_owned()),
        ..Default::default()
    }
    .validate(&empty_password_secrets(), None)
    .err()
    .expect("empty password secret rejected");
    assert!(err.contains("missing or empty"));

    let err = ExtConfig {
        jid: Some("tau@example.org".to_owned()),
        password_secret: Some("xmpp_password".to_owned()),
        ..Default::default()
    }
    .validate(&secrets(), None)
    .err()
    .expect("empty allowlist rejected");
    assert!(err.contains("allowed_jids"));

    let err = ExtConfig {
        jid: Some("tau@example.org".to_owned()),
        password_secret: Some("xmpp_password".to_owned()),
        allowed_jids: vec!["me@example.org".to_owned()],
        default_recipient: Some("other@example.org".to_owned()),
        routing: RoutingConfig {
            mode: Some("direct_resource".to_owned()),
        },
        ..Default::default()
    }
    .validate(&secrets(), None)
    .err()
    .expect("default recipient not allowed");
    assert!(err.contains("default_recipient"));

    let err = ExtConfig {
        jid: Some("tau@example.org".to_owned()),
        password_secret: Some("xmpp_password".to_owned()),
        allowed_jids: vec!["me@example.org".to_owned()],
        default_recipient: Some("me@example.org".to_owned()),
        routing: RoutingConfig {
            mode: Some("carrier_pigeon".to_owned()),
        },
        ..Default::default()
    }
    .validate(&secrets(), None)
    .err()
    .expect("unsupported routing mode rejected");
    assert!(err.contains("routing.mode"));

    let err = ExtConfig {
        jid: Some("tau@example.org".to_owned()),
        password_secret: Some("xmpp_password".to_owned()),
        allowed_jids: vec!["me@example.org".to_owned()],
        default_recipient: Some("me@example.org".to_owned()),
        routing: RoutingConfig {
            mode: Some("muc".to_owned()),
        },
        ..Default::default()
    }
    .validate(&secrets(), None)
    .err()
    .expect("muc service required");
    assert!(err.contains("muc.service"));

    let err = ExtConfig {
        jid: Some("tau@example.org".to_owned()),
        password_secret: Some("xmpp_password".to_owned()),
        allowed_jids: vec!["me@example.org".to_owned()],
        default_recipient: Some("me@example.org".to_owned()),
        routing: RoutingConfig {
            mode: Some("direct_resource".to_owned()),
        },
        max_message_bytes: Some(0),
        ..Default::default()
    }
    .validate(&secrets(), None)
    .err()
    .expect("zero limit rejected");
    assert!(err.contains("max_message_bytes"));

    let err = ExtConfig {
        jid: Some("tau@example.org".to_owned()),
        password_secret: Some("xmpp_password".to_owned()),
        allowed_jids: vec!["me@example.org".to_owned()],
        default_recipient: Some("me@example.org".to_owned()),
        routing: RoutingConfig {
            mode: Some("direct_resource".to_owned()),
        },
        max_message_bytes: Some(MAX_MESSAGE_LIMIT + 1),
        ..Default::default()
    }
    .validate(&secrets(), None)
    .err()
    .expect("oversized limit rejected");
    assert!(err.contains("max_message_bytes"));

    let err = ExtConfig {
        jid: Some("tau@example.org".to_owned()),
        password_secret: Some("xmpp_password".to_owned()),
        allowed_jids: vec!["me@example.org".to_owned()],
        default_recipient: Some("me@example.org".to_owned()),
        routing: RoutingConfig {
            mode: Some("muc".to_owned()),
        },
        muc: MucConfigRaw {
            service: Some("room@conference.example.org/tau".to_owned()),
            ..Default::default()
        },
        ..Default::default()
    }
    .validate(&secrets(), None)
    .err()
    .expect("muc service with localpart/resource rejected");
    assert!(err.contains("domain-only"));
}

/// The send tool is gated on prior registration so an arbitrary agent cannot
/// send XMPP messages without explicitly opting into the bridge first.
#[test]
fn xmpp_send_fails_before_registration() {
    let (ext, rx, _bridge) = extension();
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("hi")));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("registration tool"));
}

/// Registering an agent starts the XMPP bridge lazily and records only
/// in-memory conversation state for the current Tau process.
#[test]
fn xmpp_register_true_registers_agent_and_starts_bridge() {
    let (ext, rx, bridge) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    assert_eq!(*bridge.started.lock().expect("lock"), 1);
    let state = ext.state.lock().expect("lock");
    assert!(state.registered_agents.contains_key(&agent_id("agent-1")));
    assert!(state.conversations.contains_key(&agent_id("agent-1")));
}

/// Explicit unregister revokes local authority and reports success even when
/// remote cleanup cannot be enqueued.
#[test]
fn unregister_succeeds_after_local_revocation_when_cleanup_fails() {
    let (ext, rx, bridge) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("register progress");
    let _result = rx.recv().expect("register result");
    *bridge.unregister_error.lock().expect("lock") = Some("forced cleanup failure".to_owned());

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(false)));

    let _progress = rx.recv().expect("unregister progress");
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("unregister result") else {
        panic!("emit")
    };
    assert!(matches!(
        emit.event.as_ref(),
        Event::ToolResultReported(result)
            if result.result == CborValue::Text("unregistered from XMPP messages".to_owned())
    ));
    assert!(
        !ext.state
            .lock()
            .expect("lock")
            .registered_agents
            .contains_key(&agent_id("agent-1"))
    );
}

/// Stale worker cleanup retires only its exact generation and cannot remove a
/// newer registration for the same agent id.
#[test]
fn stale_cleanup_cannot_remove_newer_worker_generation() {
    let (tx, _rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let agent = agent_id("agent-1");
    let stale = worker.authority.reserve(agent.clone());
    worker.registration_leases.insert(agent.clone(), stale);
    let current = worker.authority.reserve(agent.clone());
    assert!(worker.authority.activate(&agent, current));
    worker.registration_leases.insert(agent.clone(), current);
    worker.conversations.insert(
        agent.clone(),
        Conversation::Direct {
            full_jid: Jid::new("tau@example.org/current").expect("jid"),
        },
    );

    assert!(worker.retire_registration(&agent, stale).is_none());
    assert_eq!(worker.registration_leases.get(&agent), Some(&current));
    assert!(worker.conversations.contains_key(&agent));
}

/// A superseded registration completion cannot activate or revoke the newer
/// generation for the same agent.
#[test]
fn stale_registration_completion_cannot_change_newer_generation() {
    let authority = RegistrationAuthority::default();
    let agent = agent_id("agent-1");
    let stale = authority.reserve(agent.clone());
    let current = authority.reserve(agent.clone());

    assert!(!authority.activate(&agent, stale));
    assert!(!authority.revoke(&agent, stale));
    assert!(authority.activate(&agent, current));
    assert_eq!(
        authority.publish_if_active(&agent, current, || "published"),
        Some("published")
    );
}

/// A real Extension completion delayed behind a newer registration cannot
/// install reader state or clean up that newer generation.
#[test]
fn stale_extension_registration_completion_cannot_replace_newer_generation() {
    let (tx, rx) = mpsc::channel();
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    bridge.block_next_registration();
    let ext = Extension::new(bridge.clone(), tx);
    ext.apply_config(cfg()).expect("apply config");
    ext.state.lock().expect("lock").current_session_id = Some(
        "session-1"
            .parse()
            .expect("known-safe SessionId must be valid"),
    );

    std::thread::scope(|scope| {
        let stale = scope.spawn(|| {
            ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
        });
        let _progress = rx.recv().expect("stale progress");
        bridge.wait_for_blocked_registration();
        let current = scope.spawn(|| {
            ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
        });
        let _progress = rx.recv().expect("current progress");
        let _result = rx.recv().expect("current result");
        current.join().expect("current registration");
        let current_lease = ext
            .state
            .lock()
            .expect("lock")
            .registered_agents
            .get(&agent_id("agent-1"))
            .copied()
            .expect("current reader lease");

        bridge.release_registration();
        stale.join().expect("stale registration");
        let _stale_result = rx.recv().expect("stale result");

        assert_eq!(
            ext.state
                .lock()
                .expect("lock")
                .registered_agents
                .get(&agent_id("agent-1")),
            Some(&current_lease)
        );
        assert_eq!(bridge.cleanup_leases.lock().expect("lock").len(), 1);
    });
}

/// Local revocation rejects an inbound stanza even while the worker still has
/// the old route because its queued cleanup command has not been selected.
#[test]
fn revocation_precedes_worker_cleanup_command_selection() {
    let (tx, rx) = mpsc::channel();
    let mut config = cfg();
    config.routing_mode = RoutingMode::DirectResource;
    let mut worker = WorkerState::new(config, tx, shutdown_signal(), test_authority());
    let agent = agent_id("agent-1");
    let bound = Jid::new("tau@example.org/tau-resource").expect("jid");
    worker.bound_jid = Some(bound.clone());
    worker.conversations.insert(
        agent.clone(),
        Conversation::Direct {
            full_jid: bound.clone(),
        },
    );
    let lease = activate_worker_agent(&mut worker, agent.clone());
    assert!(worker.authority.revoke(&agent, lease));

    let mut message = Message::chat(bound.clone()).with_body(Lang::new(), "hello".to_owned());
    message.from = Some(Jid::new("me@example.org/dino").expect("jid"));
    message.to = Some(bound);
    worker.handle_stanza(message.into());

    assert!(worker.conversations.contains_key(&agent));
    assert!(rx.try_recv().is_err());
}

/// Unload and session shutdown remove reader-visible leases before cleanup and
/// never leave the retired registration locally sendable.
#[test]
fn unload_and_session_shutdown_revoke_reader_registration() {
    let (ext, rx, _bridge) = extension();
    for agent in ["agent-1", "agent-2"] {
        ext.dispatch_tool(tool(REGISTER_TOOL_NAME, agent, bool_args(true)));
        let _progress = rx.recv().expect("register progress");
        let _result = rx.recv().expect("register result");
    }
    unload_agent(&ext, agent_id("agent-1"));
    shutdown_session(
        &ext,
        "session-1"
            .parse()
            .expect("known-safe SessionId must be valid"),
    );

    let state = ext.state.lock().expect("lock");
    assert!(state.registered_agents.is_empty());
    assert!(state.conversations.is_empty());
}

/// Every lifecycle retirement path revokes the active lease before enqueueing
/// cleanup and passes that same exact lease to the bridge.
#[test]
fn lifecycle_retirement_paths_revoke_and_enqueue_exact_lease() {
    for retire in ["unload", "shutdown", "rollover", "disconnect"] {
        let (ext, rx, bridge) = extension();
        ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
        let _progress = rx.recv().expect("register progress");
        let _result = rx.recv().expect("register result");
        let agent = agent_id("agent-1");
        let lease = ext
            .state
            .lock()
            .expect("lock")
            .registered_agents
            .get(&agent)
            .copied()
            .expect("active lease");

        match retire {
            "unload" => unload_agent(&ext, agent.clone()),
            "shutdown" => shutdown_session(
                &ext,
                "session-1"
                    .parse()
                    .expect("known-safe SessionId must be valid"),
            ),
            "rollover" => {
                ext.revoke_all();
                ext.state.lock().expect("lock").current_session_id = Some(
                    "session-2"
                        .parse()
                        .expect("known-safe SessionId must be valid"),
                );
            }
            "disconnect" => ext.revoke_all(),
            _ => unreachable!("known test case"),
        }

        assert_eq!(
            ext.authority.publish_if_active(&agent, lease, || ()),
            None,
            "{retire} retained local publication authority"
        );
        assert_eq!(
            *bridge.cleanup_leases.lock().expect("lock"),
            vec![(agent, lease)],
            "{retire} enqueued the wrong cleanup generation"
        );
    }
}

/// Before the worker starts, a new invalid `Configure` replaces the previous
/// accepted configuration with no usable configuration. Otherwise a visible
/// config error could still be followed by registration with stale credentials,
/// allowlists, or routing policy.
#[test]
fn invalid_configure_before_start_clears_stale_config() {
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            HarnessOutputMessage::Configure(configure_from_json(serde_json::json!({
                "jid": "tau@example.org",
                "password_secret": "xmpp_password",
                "default_recipient": "me@example.org",
                "routing": { "mode": "direct_resource" }
            }))),
            session_started_message("session-1"),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-1",
                bool_args(true),
            ))),
        ],
        bridge.clone(),
    );

    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::ConfigError(error) if error.message.contains("allowed_jids")
    )));
    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Emit(emit)
            if matches!(
                emit.event.as_ref(),
                Event::ToolErrorReported(error)
                    if error.tool_name.as_str() == REGISTER_TOOL_NAME
                        && error.message.contains("not configured")
            )
    )));
    assert_eq!(*bridge.started.lock().expect("lock"), 0);
}

/// Runtime configuration is captured by the worker at bridge startup. A later
/// harness reconfiguration must not silently update only the tool-side state
/// while the live XMPP worker keeps using the old credentials, allowlist, or
/// routing mode.
#[test]
fn configure_after_bridge_start_reports_config_error() {
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            session_started_message("session-1"),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-1",
                bool_args(true),
            ))),
            malformed_config_message(),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                SEND_TOOL_NAME,
                "agent-1",
                message_args("reply"),
            ))),
        ],
        bridge.clone(),
    );

    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::ConfigError(error) if error.message.contains("cannot be changed")
    )));
    assert!(!frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::ConfigError(error) if error.message.contains("unknown_field")
    )));
    assert_eq!(*bridge.started.lock().expect("lock"), 1);
    assert_eq!(
        *bridge.sent.lock().expect("lock"),
        vec![(agent_id("agent-1"), "[agent-1] reply".to_owned())]
    );
}

/// Replayed `session.started` events are intentionally accepted so a resumed
/// session can restore the lifecycle state required before the next live
/// registration.
#[test]
fn run_replayed_session_started_enables_later_register() {
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            HarnessOutputMessage::deliver_replay(
                tau_proto::UnixMicros::new(1_700_000_000_000_000),
                Event::SessionStarted(tau_proto::SessionStarted {
                    session_id: "session-1"
                        .parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                    reason: tau_proto::SessionStartReason::Resume,
                }),
            ),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-1",
                bool_args(true),
            ))),
        ],
        bridge.clone(),
    );

    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Emit(emit)
            if matches!(
                emit.event.as_ref(),
                Event::ToolResultReported(result)
                    if result.tool_name.as_str() == REGISTER_TOOL_NAME
            )
    )));
    assert_eq!(*bridge.started.lock().expect("lock"), 1);
}

/// Replayed unload/shutdown lifecycle facts are ignored so historical session
/// cleanup cannot clear a live registration restored by subsequent activity.
#[test]
fn run_replayed_unload_and_shutdown_do_not_clear_registration() {
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    run_protocol_messages(
        &[
            valid_config_message(),
            session_started_message("session-1"),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-1",
                bool_args(true),
            ))),
            HarnessOutputMessage::deliver_replay(
                tau_proto::UnixMicros::new(1_700_000_000_000_000),
                Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                    session_id: "session-1"
                        .parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                    agent_id: agent_id("agent-1"),
                }),
            ),
            HarnessOutputMessage::deliver_replay(
                tau_proto::UnixMicros::new(1_700_000_000_000_001),
                Event::SessionShutdown(tau_proto::SessionShutdown {
                    session_id: "session-1"
                        .parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                }),
            ),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                SEND_TOOL_NAME,
                "agent-1",
                message_args("after replay"),
            ))),
        ],
        bridge.clone(),
    );

    assert_eq!(
        *bridge.sent.lock().expect("lock"),
        vec![(agent_id("agent-1"), "[agent-1] after replay".to_owned())]
    );
}

/// `xmpp_register` waits for bridge readiness before creating the server-backed
/// conversation, preventing early startup races from surfacing as immediate
/// "not online yet" tool failures.
#[test]
fn xmpp_register_waits_for_online_readiness() {
    let (tx, rx) = mpsc::channel();
    let bridge = FakeBridge::new();
    let ext = Extension::new(bridge.clone(), tx);
    ext.apply_config(cfg()).expect("apply config");
    ext.state.lock().expect("lock").current_session_id = Some(
        "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
    );

    std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
        });
        let _progress = rx.recv().expect("progress");
        assert_eq!(bridge.wait_for_wait_calls(1), vec![Duration::from_secs(30)]);
        assert!(bridge.registered.lock().expect("lock").is_empty());
        bridge.set_ready(true);
        handle.join().expect("register thread");
    });

    let _result = rx.recv().expect("result");
    assert!(
        bridge
            .registered
            .lock()
            .expect("lock")
            .contains_key(&agent_id("agent-1"))
    );
}

/// Register readiness timeout errors are surfaced directly and do not leave a
/// partially registered conversation in extension or bridge state.
#[test]
fn xmpp_register_readiness_timeout_is_clear_and_does_not_register() {
    let (tx, rx) = mpsc::channel();
    let bridge = FakeBridge::new();
    bridge.set_readiness_error(
        "xmpp connection did not become online within 30s; retry after the account connects",
    );
    let ext = Extension::new(bridge.clone(), tx);
    ext.apply_config(cfg()).expect("apply config");
    ext.state.lock().expect("lock").current_session_id = Some(
        "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
    );

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("within 30s"));
    assert!(error.message.contains("retry"));
    assert_eq!(bridge.wait_for_wait_calls(1), vec![Duration::from_secs(30)]);
    assert!(bridge.registered.lock().expect("lock").is_empty());
    let state = ext.state.lock().expect("lock");
    assert!(!state.registered_agents.contains_key(&agent_id("agent-1")));
    assert!(!state.conversations.contains_key(&agent_id("agent-1")));
}

/// Registration needs an active Tau session for lifecycle cleanup and template
/// context; it should fail before starting XMPP if the extension has not
/// observed `session.started`.
#[test]
fn xmpp_register_requires_active_session_before_starting_bridge() {
    let (tx, rx) = mpsc::channel();
    let bridge = FakeBridge::new();
    let ext = Extension::new(bridge.clone(), tx);
    ext.apply_config(cfg()).expect("apply config");

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("active Tau session"));
    assert_eq!(*bridge.started.lock().expect("lock"), 0);
}

/// In MUC mode, two agents in the same Tau session can register at the same
/// time and receive separate stable room addresses keyed by globally unique
/// agent ids.
#[test]
fn xmpp_register_allows_two_muc_agents_in_same_session() {
    let (ext, rx, bridge) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _ = rx.recv();
    let _ = rx.recv();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));
    let _ = rx.recv();
    let _ = rx.recv();

    let registered = bridge.registered.lock().expect("lock");
    let agent_1 = registered.get(&agent_id("agent-1")).expect("agent 1");
    let agent_2 = registered.get(&agent_id("agent-2")).expect("agent 2");
    assert_ne!(agent_1, agent_2);
    assert!(agent_1.starts_with("agent-1-"), "{agent_1}");
    assert!(agent_1.ends_with("@conference.example.org"));
    assert_eq!(
        agent_1.len(),
        "agent-1-".len() + 8 + "@conference.example.org".len()
    );
    assert!(agent_2.starts_with("agent-2-"));
    assert!(agent_2.ends_with("@conference.example.org"));
    assert_eq!(
        agent_2.len(),
        "agent-2-".len() + 8 + "@conference.example.org".len()
    );
}

/// `xmpp_register` rejects unexpected arguments so registration cannot grow a
/// hidden model-chosen destination surface outside the declared schema.
#[test]
fn xmpp_register_rejects_unknown_arguments() {
    let (ext, rx, _bridge) = extension();
    ext.dispatch_tool(tool(
        REGISTER_TOOL_NAME,
        "agent-1",
        bool_args_with_extra(true),
    ));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("destination"));
}

/// After registration, `xmpp_send` sends to the fixed conversation and prefixes
/// the text with the stable agent id rather than accepting a destination JID.
#[test]
fn xmpp_send_uses_registered_conversation_without_destination_arg() {
    let (ext, rx, bridge) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _ = rx.recv();
    let _ = rx.recv();
    let conversation = ext
        .state
        .lock()
        .expect("lock")
        .conversations
        .get(&agent_id("agent-1"))
        .expect("conversation")
        .clone();
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("hello")));
    let _progress = rx.recv().expect("progress");
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("message.sent_reported") else {
        panic!("emit")
    };
    assert!(!emit.persist);
    let Event::MessageSentReported(report) = *emit.event else {
        panic!("message.sent_reported report")
    };
    assert_eq!(report.publisher_extension_id.as_str(), "std-xmpp");
    assert_eq!(report.agent_id.as_str(), "agent-1");
    assert_eq!(report.text, "hello");
    assert_eq!(
        report.conversation.expect("conversation").stable_id,
        conversation
    );
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("result") else {
        panic!("emit")
    };
    assert!(matches!(*emit.event, Event::ToolResultReported(_)));
    let sent = bridge.sent.lock().expect("lock");
    assert_eq!(sent[0], (agent_id("agent-1"), "[agent-1] hello".to_owned()));
}

/// Tau's conservative 4096-byte policy must remain inclusive so a message that
/// exactly fits is sent as one unnumbered body.
#[test]
fn xmpp_send_keeps_exact_outbound_body_limit_in_one_message() {
    let (ext, rx, bridge) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("registration progress");
    let _result = rx.recv().expect("registration result");

    let prefix = "[agent-1] ";
    let message = "a".repeat(OUTBOUND_BODY_LIMIT_BYTES - prefix.len());
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args(&message)));
    let _progress = rx.recv().expect("send progress");
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("message.sent_reported") else {
        panic!("emit")
    };
    let Event::MessageSentReported(report) = *emit.event else {
        panic!("message.sent_reported report")
    };
    assert_eq!(report.text, message);
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("send result") else {
        panic!("emit")
    };
    assert!(matches!(*emit.event, Event::ToolResultReported(_)));

    let sent = bridge.sent.lock().expect("lock");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1.len(), OUTBOUND_BODY_LIMIT_BYTES);
    assert_eq!(sent[0].1, format!("{prefix}{message}"));
}

/// A body one byte beyond the effective outbound limit must be split with
/// visible numbering rather than silently losing its final byte.
#[test]
fn xmpp_send_numbers_message_beyond_outbound_body_limit() {
    let (ext, rx, bridge) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("registration progress");
    let _result = rx.recv().expect("registration result");

    let prefix = "[agent-1] ";
    let message = "a".repeat(OUTBOUND_BODY_LIMIT_BYTES - prefix.len() + 1);
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args(&message)));
    let _progress = rx.recv().expect("send progress");
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("message.sent_reported") else {
        panic!("emit")
    };
    let Event::MessageSentReported(report) = *emit.event else {
        panic!("message.sent_reported report")
    };
    assert_eq!(report.text, message);
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("send result") else {
        panic!("emit")
    };
    assert!(matches!(*emit.event, Event::ToolResultReported(_)));

    let sent = bridge.sent.lock().expect("lock");
    assert_eq!(sent.len(), 2);
    assert!(sent[0].1.starts_with("[agent-1] [part 1/2] "));
    assert!(sent[1].1.starts_with("[agent-1] [part 2/2] "));
    assert!(
        sent.iter()
            .all(|(_, text)| text.len() <= OUTBOUND_BODY_LIMIT_BYTES)
    );
    assert_eq!(reconstruct_multipart_payload(&sent, prefix), message);
    assert!(rx.try_recv().is_err());
}

/// Multipart splitting must preserve every byte of multibyte UTF-8 text while
/// keeping each numbered body within the interoperability limit.
#[test]
fn outbound_message_parts_split_only_at_utf8_boundaries() {
    let agent_id = agent_id("agent-1");
    let message = "🙂é".repeat(1_000);
    let parts = outbound_message_parts(&agent_id, &message).expect("split message");
    assert!(parts.len() > 1);
    assert!(
        parts
            .iter()
            .all(|part| part.len() <= OUTBOUND_BODY_LIMIT_BYTES)
    );

    let prefix = "[agent-1] ";
    let sent = parts
        .into_iter()
        .map(|part| (agent_id.clone(), part))
        .collect::<Vec<_>>();
    let reconstructed = reconstruct_multipart_payload(&sent, prefix);
    assert_eq!(reconstructed, message);
}

/// Denominator-width recomputation must converge when the payload grows from
/// nine to ten parts without overflowing a body or misnumbering a marker.
#[test]
fn outbound_message_parts_converge_across_ten_part_boundary() {
    let agent_id = agent_id("agent-1");
    let prefix = "[agent-1] ";
    let first_pass_nine_capacity = (1..=9)
        .map(|index| OUTBOUND_BODY_LIMIT_BYTES - prefix.len() - format!("[part {index}/2] ").len())
        .sum::<usize>();
    let message = "a".repeat(first_pass_nine_capacity + 1);
    let parts = outbound_message_parts(&agent_id, &message).expect("split message");

    assert_eq!(parts.len(), 10);
    for (index, part) in parts.iter().enumerate() {
        assert!(part.starts_with(&format!("{prefix}[part {}/10] ", index + 1)));
        assert!(part.len() <= OUTBOUND_BODY_LIMIT_BYTES);
    }
    let sent = parts
        .into_iter()
        .map(|part| (agent_id.clone(), part))
        .collect::<Vec<_>>();
    assert_eq!(reconstruct_multipart_payload(&sent, prefix), message);
}

/// A failure after the first multipart write must report partial delivery,
/// stop sending, and avoid submitting an all-message `message.sent_reported`
/// report.
#[test]
fn xmpp_send_reports_partial_multipart_failure_without_sent_report() {
    let (ext, rx, bridge) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("registration progress");
    let _result = rx.recv().expect("registration result");
    bridge.set_send_error_after(1, "xmpp transport failed");

    let message = "a".repeat(OUTBOUND_BODY_LIMIT_BYTES);
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args(&message)));
    let _progress = rx.recv().expect("send progress");
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("tool error") else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    assert_eq!(
        error.message,
        "failed to send XMPP message part 2/2 after 1 complete part(s): xmpp transport failed"
    );
    assert_eq!(bridge.sent.lock().expect("lock").len(), 1);
    assert!(
        rx.try_recv().is_err(),
        "unexpected message.sent_reported or later write"
    );
}

fn reconstruct_multipart_payload(sent: &[(AgentId, String)], prefix: &str) -> String {
    sent.iter()
        .map(|(_, part)| {
            let numbered = part.strip_prefix(prefix).expect("agent prefix");
            numbered
                .split_once("] ")
                .map(|(_, payload)| payload)
                .expect("part marker")
        })
        .collect()
}

/// An XMPP transport send failure must return a tool error without submitting a
/// preceding or later `message.sent_reported` report.
#[test]
fn xmpp_send_transport_failure_does_not_submit_sent_report() {
    let (ext, rx, bridge) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("registration progress");
    let _result = rx.recv().expect("registration result");
    bridge.set_send_error("xmpp transport failed");

    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("hello")));
    let _progress = rx.recv().expect("send progress");
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("tool error") else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    assert_eq!(error.message, "xmpp transport failed");
    assert!(
        rx.try_recv().is_err(),
        "unexpected message.sent_reported after failure"
    );
}

/// `xmpp_send` waits for the already-started bridge to become ready again
/// before handing the message to the worker, which covers reconnects after a
/// successful registration without requiring agents to implement manual retry
/// loops.
#[test]
fn xmpp_send_waits_for_online_readiness_after_registration() {
    let (ext, rx, bridge) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _ = rx.recv();
    let _ = rx.recv();

    bridge.set_ready(false);
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("hello")));
        });
        let _progress = rx.recv().expect("progress");
        assert_eq!(
            bridge.wait_for_wait_calls(2),
            vec![Duration::from_secs(30), Duration::from_secs(30)]
        );
        assert!(bridge.sent.lock().expect("lock").is_empty());
        bridge.set_ready(true);
        handle.join().expect("send thread");
    });

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("message.sent_reported") else {
        panic!("emit")
    };
    assert!(matches!(*emit.event, Event::MessageSentReported(_)));
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("result") else {
        panic!("emit")
    };
    assert!(matches!(*emit.event, Event::ToolResultReported(_)));
    let sent = bridge.sent.lock().expect("lock");
    assert_eq!(sent[0], (agent_id("agent-1"), "[agent-1] hello".to_owned()));
}

/// Readiness waits are deliberately bounded to the 30-second user-facing retry
/// window, avoiding hidden unbounded tool calls when the XMPP account never
/// connects.
#[test]
fn online_readiness_wait_is_bounded_to_thirty_seconds() {
    assert_eq!(ONLINE_WAIT_TIMEOUT, Duration::from_secs(30));
}

/// Harness disconnect is the authoritative lifecycle signal; the extension must
/// stop processing further input and ask the XMPP bridge to clean up rooms.
#[test]
fn harness_disconnect_stops_extension_and_shuts_down_bridge() {
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    run_protocol_messages(
        &[
            valid_config_message(),
            session_started_message("session-1"),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-1",
                bool_args(true),
            ))),
            HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
                reason: Some("test shutdown".to_owned()),
            }),
        ],
        bridge.clone(),
    );

    assert_eq!(*bridge.shutdowns.lock().expect("lock"), 1);
    assert_eq!(bridge.cleanup_leases.lock().expect("lock").len(), 1);
}

/// A new live session id retires every registration from the previous session
/// before later tools can observe it.
#[test]
fn session_rollover_revokes_active_registration() {
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            session_started_message("session-1"),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-1",
                bool_args(true),
            ))),
            session_started_message("session-2"),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                SEND_TOOL_NAME,
                "agent-1",
                message_args("must fail"),
            ))),
        ],
        bridge.clone(),
    );

    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Emit(emit)
            if matches!(
                emit.event.as_ref(),
                Event::ToolErrorReported(error) if error.message.contains("registration tool")
            )
    )));
    assert_eq!(bridge.cleanup_leases.lock().expect("lock").len(), 1);
}

/// A disconnect must clear the cached online marker so later register/send
/// commands do not silently reuse stale readiness from an earlier stream.
#[test]
fn disconnected_state_requires_fresh_online_readiness() {
    let (tx, _rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    worker.bound_jid = Some(Jid::new("tau@example.org/tau-resource").expect("jid"));
    retain_muc_identity(
        &mut worker,
        Jid::new("room@conference.example.org/alice").expect("jid"),
        Jid::new("me@example.org/dino").expect("jid"),
    );

    worker.handle_disconnected();

    assert!(worker.bound_jid.is_none());
    assert_eq!(worker.muc_presence_cache.total_len(), 0);
}

/// `xmpp_send` rejects unexpected arguments such as a destination JID so future
/// protocol or schema changes cannot accidentally make model-chosen recipients
/// meaningful.
#[test]
fn xmpp_send_rejects_unknown_destination_argument() {
    let (ext, rx, _bridge) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _ = rx.recv();
    let _ = rx.recv();
    ext.dispatch_tool(tool(
        SEND_TOOL_NAME,
        "agent-1",
        message_args_with_destination("hello"),
    ));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("destination"));
}

fn worker_with_muc_agent() -> (
    WorkerState,
    mpsc::Receiver<HarnessInputMessage>,
    BareJid,
    Jid,
) {
    let (tx, rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let room = Jid::new("tau-agent-1@conference.example.org")
        .expect("jid")
        .to_bare();
    let occupant = Jid::new("tau-agent-1@conference.example.org/alice").expect("jid");
    worker
        .room_to_agent
        .insert(room.clone(), agent_id("agent-1"));
    worker.conversations.insert(
        agent_id("agent-1"),
        Conversation::Muc {
            room: room.clone(),
            nick: "tau-self".to_owned(),
        },
    );
    activate_worker_agent(&mut worker, agent_id("agent-1"));
    (worker, rx, room, occupant)
}

fn muc_message(from: Jid, body: &str) -> Stanza {
    let mut message =
        Message::groupchat(Jid::new("tau-agent-1@conference.example.org").expect("jid"))
            .with_body(Lang::new(), body.to_owned());
    message.from = Some(from);
    message.into()
}

/// Retain one test MUC identity through the same bounded cache used by
/// presence.
fn retain_muc_identity(worker: &mut WorkerState, occupant: Jid, real_jid: Jid) {
    let real_jid = real_jid
        .try_into_full()
        .unwrap_or_else(|bare| full_jid(&format!("{bare}/test-resource")));
    assert_eq!(
        worker
            .muc_presence_cache
            .admit(occupant.try_into_full().expect("full occupant"), real_jid,),
        Admission::Retained
    );
}

fn delayed_muc_message(from: Jid, body: &str) -> Stanza {
    let mut message =
        Message::groupchat(Jid::new("tau-agent-1@conference.example.org").expect("jid"))
            .with_body(Lang::new(), body.to_owned());
    message.from = Some(from);
    message.payloads.push(
        "<delay xmlns='urn:xmpp:delay' from='conference.example.org' stamp='2026-06-18T00:00:00Z'/>"
            .parse()
            .expect("delay payload"),
    );
    message.into()
}

/// MUC join confirmation must notice status 201 so registration can unlock a
/// newly-created room with an instant-room owner configuration before reporting
/// success to the agent.
#[test]
fn muc_self_presence_status_201_requires_instant_room_setup() {
    let occupant = Jid::new("tau-agent-1@conference.example.org/tau-self").expect("jid");
    let mut presence = Presence::available();
    presence.from = Some(occupant);
    presence.payloads.push(
        MucUser::new()
            .with_statuses(vec![MucStatus::SelfPresence, MucStatus::RoomHasBeenCreated])
            .into(),
    );

    let join = MucJoin::from_self_presence(&presence).expect("self-presence");

    assert!(join.created);
    assert!(join.statuses.contains(&MucStatus::RoomHasBeenCreated));
}

/// MUC registration must fail loudly on a matching join presence error instead
/// of storing a room and claiming success for a locked or policy-rejected room.
#[test]
fn muc_join_presence_error_is_reported() {
    let occupant = Jid::new("tau-agent-1@conference.example.org/tau-self").expect("jid");
    let mut presence = Presence::error();
    presence.from = Some(occupant);
    presence.payloads.push(
        StanzaError::new(
            path_xmpp_parsers_stanza_error::ErrorType::Cancel,
            path_xmpp_parsers_stanza_error::DefinedCondition::ItemNotFound,
            "",
            "room locked",
        )
        .into(),
    );

    let err = MucJoin::from_self_presence(&presence).expect_err("join error");

    assert!(err.contains("MUC join rejected"));
    assert!(err.contains("ItemNotFound"));
}

/// A matching unavailable presence is not a successful MUC join confirmation:
/// treating it as success would let registration continue after the server has
/// removed Tau's room occupant.
#[test]
fn muc_unavailable_self_presence_is_not_join_success() {
    let occupant = Jid::new("tau-agent-1@conference.example.org/tau-self").expect("jid");
    let mut presence = Presence::unavailable();
    presence.from = Some(occupant);

    let err = MucJoin::from_self_presence(&presence).expect_err("unavailable rejected");

    assert!(err.contains("did not succeed"));
    assert!(err.contains("Unavailable"));
}

/// The instant-room setup IQ uses the XEP-0045 owner namespace with an empty
/// XEP-0004 submit form, which unlocks a new room using server defaults without
/// pretending to configure privacy or affiliations.
#[test]
fn instant_room_config_query_uses_owner_submit_form() {
    let query = instant_room_config_query();
    let form = query.children().next().expect("form child");

    assert_eq!(query.name(), "query");
    assert_eq!(query.ns(), MUC_OWNER_NS);
    assert_eq!(form.name(), "x");
    assert_eq!(form.ns(), xmpp_parsers::ns::DATA_FORMS);
    assert_eq!(form.attr("type"), Some("submit"));
}

/// Pending MUC joins are separate from routable room maps so a room that fails
/// setup can be cleaned up without accepting prompts from it.
#[test]
fn pending_muc_join_is_not_routable_and_can_be_removed() {
    let (tx, _rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let room = Jid::new("tau-agent-1@conference.example.org")
        .expect("jid")
        .to_bare();
    worker.pending_muc_joins.insert(
        agent_id("agent-1"),
        MucOccupant::new(room.clone(), "tau-self".to_owned()),
    );

    assert!(!worker.room_to_agent.contains_key(&room));
    assert!(!worker.conversations.contains_key(&agent_id("agent-1")));
    assert!(
        worker
            .pending_muc_joins
            .remove(&agent_id("agent-1"))
            .is_some()
    );
}

/// Join confirmation correlation is exact to the room/nick occupant JID so
/// unrelated room presence cannot satisfy a pending registration.
#[test]
fn muc_join_presence_correlation_requires_exact_room_and_nick() {
    let room = Jid::new("tau-agent-1@conference.example.org")
        .expect("jid")
        .to_bare();
    let occupant = muc_occupant_jid(&room, "tau-self").expect("occupant jid");
    let mut matching = Presence::available();
    matching.from = Some(occupant);
    let mut other_nick = Presence::available();
    other_nick.from = Some(Jid::new("tau-agent-1@conference.example.org/alice").expect("jid"));
    let mut other_room = Presence::available();
    other_room.from = Some(Jid::new("other@conference.example.org/tau-self").expect("jid"));

    assert!(muc_presence_from(
        &matching,
        matching.from.as_ref().expect("from")
    ));
    assert!(!muc_presence_from(
        &other_nick,
        matching.from.as_ref().expect("from")
    ));
    assert!(!muc_presence_from(
        &other_room,
        matching.from.as_ref().expect("from")
    ));
}

/// Build one MUC-user presence assertion for cache admission tests.
fn muc_identity_presence(from: Jid, real_jid: &str, type_: PresenceType) -> Presence {
    let mut presence = Presence::new(type_);
    presence.from = Some(from);
    presence.payloads.push(
        format!(
            "<x xmlns='http://jabber.org/protocol/muc#user'><item affiliation='member' role='participant' jid='{real_jid}'/></x>"
        )
        .parse()
        .expect("muc-user payload"),
    );
    presence
}

/// Overflow one active room through production presence ingress.
fn overflow_active_room(worker: &mut WorkerState, room: &BareJid) {
    for index in 0..=MAX_MUC_OCCUPANTS_PER_ROOM {
        worker.handle_presence(muc_identity_presence(
            Jid::new(&format!("{room}/nick-{index}")).expect("jid"),
            &format!("user-{index}@example.org/resource"),
            PresenceType::None,
        ));
    }
}

/// Parse a full JID for typed cache unit tests.
fn full_jid(value: &str) -> xmpp_parsers::jid::FullJid {
    Jid::new(value)
        .expect("jid")
        .try_into_full()
        .expect("full jid")
}

/// Presence from irrelevant rooms, bare senders, and non-available types must
/// never create authentication state that could become authoritative later.
#[test]
fn muc_presence_cache_accepts_only_available_full_jids_from_tracked_rooms() {
    let (mut worker, _rx, room, _occupant) = worker_with_muc_agent();
    let irrelevant = Jid::new("other@conference.example.org/alice").expect("jid");
    worker.handle_presence(muc_identity_presence(
        irrelevant,
        "me@example.org/dino",
        PresenceType::None,
    ));
    assert_eq!(worker.muc_presence_cache.total_len(), 0);

    worker.handle_presence(muc_identity_presence(
        room.clone().into(),
        "me@example.org/dino",
        PresenceType::None,
    ));
    for type_ in [
        PresenceType::Probe,
        PresenceType::Subscribe,
        PresenceType::Subscribed,
        PresenceType::Unsubscribe,
        PresenceType::Unsubscribed,
    ] {
        worker.handle_presence(muc_identity_presence(
            Jid::new("tau-agent-1@conference.example.org/alice").expect("jid"),
            "me@example.org/dino",
            type_,
        ));
    }
    assert_eq!(worker.muc_presence_cache.total_len(), 0);

    let occupant = Jid::new("tau-agent-1@conference.example.org/alice").expect("jid");
    worker.handle_presence(muc_identity_presence(
        occupant.clone(),
        "me@example.org/dino",
        PresenceType::None,
    ));
    assert_eq!(worker.muc_presence_cache.room_len(&room), 1);
    let mut unavailable = Presence::unavailable();
    unavailable.from = Some(occupant);
    worker.handle_presence(unavailable);
    assert_eq!(worker.muc_presence_cache.total_len(), 0);
}

/// Initial roster presence for the exact pending room must survive promotion,
/// while the same pre-join assertion from another room stays irrelevant.
#[test]
fn muc_pending_roster_mapping_routes_after_successful_promotion() {
    let (tx, rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let agent = agent_id("agent-1");
    let room = Jid::new("tau-agent-1@conference.example.org")
        .expect("jid")
        .to_bare();
    let occupant = Jid::new("tau-agent-1@conference.example.org/alice").expect("jid");
    worker.pending_muc_joins.insert(
        agent.clone(),
        MucOccupant::new(room.clone(), "tau-self".to_owned()),
    );
    worker.handle_presence(muc_identity_presence(
        Jid::new("other@conference.example.org/alice").expect("jid"),
        "mallory@example.org/resource",
        PresenceType::None,
    ));
    worker.handle_presence(muc_identity_presence(
        occupant.clone(),
        "me@example.org/dino",
        PresenceType::None,
    ));
    assert_eq!(worker.muc_presence_cache.total_len(), 1);

    worker.room_to_agent.insert(room.clone(), agent.clone());
    worker.conversations.insert(
        agent.clone(),
        Conversation::Muc {
            room: room.clone(),
            nick: "tau-self".to_owned(),
        },
    );
    worker.pending_muc_joins.remove(&agent);
    activate_worker_agent(&mut worker, agent);
    worker.handle_stanza(muc_message(occupant, "hello"));
    assert!(rx.try_recv().is_ok());
}

/// The 256th per-room mapping and replacement at capacity are accepted, while
/// a distinct 257th mapping clears and quarantines only that room.
#[test]
fn muc_presence_cache_per_room_limit_is_inclusive_and_replacement_safe() {
    let room = Jid::new("room@conference.example.org")
        .expect("jid")
        .to_bare();
    let mut cache = MucPresenceCache::default();
    for index in 0..MAX_MUC_OCCUPANTS_PER_ROOM {
        assert_eq!(
            cache.admit(
                full_jid(&format!("room@conference.example.org/nick-{index}")),
                full_jid(&format!("user-{index}@example.org/resource")),
            ),
            Admission::Retained
        );
    }
    assert_eq!(cache.room_len(&room), MAX_MUC_OCCUPANTS_PER_ROOM);
    assert_eq!(
        cache.admit(
            full_jid("room@conference.example.org/nick-0"),
            full_jid("replacement@example.org/resource"),
        ),
        Admission::Retained
    );
    assert_eq!(cache.room_len(&room), MAX_MUC_OCCUPANTS_PER_ROOM);
    assert_eq!(
        cache.admit(
            full_jid("room@conference.example.org/overflow"),
            full_jid("overflow@example.org/resource"),
        ),
        Admission::Quarantined
    );
    assert!(cache.is_quarantined(&room));
    assert_eq!(cache.room_len(&room), 0);
    assert_eq!(cache.total_len(), 0);
}

/// The 1,024th worker mapping and replacement at capacity are accepted; the
/// next room is quarantined without removing complete state from other rooms.
#[test]
fn muc_presence_cache_worker_limit_is_inclusive_and_room_isolated() {
    let mut cache = MucPresenceCache::default();
    let mut rooms = Vec::new();
    for room_index in 0..4 {
        let room = Jid::new(&format!("room-{room_index}@conference.example.org"))
            .expect("jid")
            .to_bare();
        for occupant_index in 0..MAX_MUC_OCCUPANTS_PER_ROOM {
            assert_eq!(
                cache.admit(
                    full_jid(&format!(
                        "room-{room_index}@conference.example.org/nick-{occupant_index}"
                    )),
                    full_jid(&format!(
                        "user-{room_index}-{occupant_index}@example.org/resource"
                    )),
                ),
                Admission::Retained
            );
        }
        rooms.push(room);
    }
    assert_eq!(cache.total_len(), MAX_MUC_OCCUPANTS_TOTAL);
    assert_eq!(
        cache.admit(
            full_jid("room-0@conference.example.org/nick-0"),
            full_jid("replacement@example.org/resource"),
        ),
        Admission::Retained
    );
    let overflow_room = Jid::new("overflow@conference.example.org")
        .expect("jid")
        .to_bare();
    assert_eq!(
        cache.admit(
            full_jid("overflow@conference.example.org/nick"),
            full_jid("overflow@example.org/resource"),
        ),
        Admission::Quarantined
    );
    assert!(cache.is_quarantined(&overflow_room));
    assert_eq!(cache.total_len(), MAX_MUC_OCCUPANTS_TOTAL);
    assert_eq!(cache.room_len(&rooms[0]), MAX_MUC_OCCUPANTS_PER_ROOM);
}

/// Pending-roster overflow produces the approved terminal detail and purging
/// the rolled-back pending join removes its quarantine state.
#[test]
fn muc_initial_roster_overflow_fails_registration_and_rollback_purges_state() {
    let (tx, _rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let agent = agent_id("agent-1");
    let room = Jid::new("tau-agent-1@conference.example.org")
        .expect("jid")
        .to_bare();
    worker.pending_muc_joins.insert(
        agent.clone(),
        MucOccupant::new(room.clone(), "tau-self".to_owned()),
    );
    for index in 0..=MAX_MUC_OCCUPANTS_PER_ROOM {
        worker.handle_presence(muc_identity_presence(
            Jid::new(&format!("tau-agent-1@conference.example.org/nick-{index}")).expect("jid"),
            &format!("user-{index}@example.org/resource"),
            PresenceType::None,
        ));
    }
    let mut self_presence = Presence::available();
    self_presence.from =
        Some(Jid::new("tau-agent-1@conference.example.org/tau-self").expect("jid"));
    self_presence.payloads.push(
        MucUser::new()
            .with_statuses(vec![MucStatus::SelfPresence])
            .into(),
    );
    assert_eq!(
        worker
            .handle_muc_self_presence(&room, self_presence)
            .expect_err("incomplete roster"),
        "xmpp MUC occupant roster exceeds cache limits (256 per room, 1024 total); registration was not installed"
    );
    assert!(!worker.room_to_agent.contains_key(&room));
    assert!(!worker.conversations.contains_key(&agent));
    worker.remove_conversation(&agent);
    assert!(!worker.muc_presence_cache.is_quarantined(&room));
    assert_eq!(worker.muc_presence_cache.total_len(), 0);
}

/// Quarantine must precede both real-JID and trusted-membership admission so
/// neither a formerly cached nor overflowing occupant can route a message.
#[test]
fn muc_quarantine_fails_closed_under_both_membership_modes() {
    for trust_membership in [false, true] {
        let (mut worker, rx, _room, occupant) = worker_with_muc_agent();
        worker.cfg.muc.trust_muc_membership = trust_membership;
        overflow_active_room(&mut worker, &_room);
        let overflow = Jid::new("tau-agent-1@conference.example.org/overflow").expect("jid");
        assert!(worker.muc_presence_cache.is_quarantined(&_room));
        worker.handle_stanza(muc_message(occupant, "formerly cached"));
        worker.handle_stanza(muc_message(overflow, "overflow"));
        assert!(rx.try_recv().is_err());
    }
}

/// Overflow in one active room must neither remove another room's identities
/// nor prevent that other room from routing an allowlisted message.
#[test]
fn muc_quarantine_isolates_other_active_rooms() {
    let (mut worker, rx, room_b, occupant_b) = worker_with_muc_agent();
    retain_muc_identity(
        &mut worker,
        occupant_b.clone(),
        Jid::new("me@example.org/dino").expect("jid"),
    );
    let room_a = Jid::new("tau-agent-2@conference.example.org")
        .expect("jid")
        .to_bare();
    worker
        .room_to_agent
        .insert(room_a.clone(), agent_id("agent-2"));
    overflow_active_room(&mut worker, &room_a);
    assert!(worker.muc_presence_cache.is_quarantined(&room_a));
    assert_eq!(worker.muc_presence_cache.room_len(&room_b), 1);
    worker.handle_stanza(muc_message(occupant_b, "room B remains usable"));
    assert!(rx.try_recv().is_ok());
}

/// Active overflow emits one fixed content-free warning per room and suppresses
/// repeats until a connection reset permits one new warning.
#[test]
fn muc_active_overflow_warning_is_content_free_once_per_connection() {
    let (mut worker, _rx, room, _occupant) = worker_with_muc_agent();
    let trace = TraceWriter::default();
    let captured = Arc::clone(&trace.0);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .without_time()
        .with_ansi(false)
        .with_writer(move || trace.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        for index in 0..MAX_MUC_OCCUPANTS_PER_ROOM {
            worker.handle_presence(muc_identity_presence(
                Jid::new(&format!("{room}/nick-{index}")).expect("jid"),
                &format!("user-{index}@example.org/resource"),
                PresenceType::None,
            ));
        }
        worker.handle_presence(muc_identity_presence(
            Jid::new("tau-agent-1@conference.example.org/secret-nick").expect("jid"),
            "secret-person@example.org/private-resource",
            PresenceType::None,
        ));
        worker.muc_presence_cache.begin_join(&room);
        overflow_active_room(&mut worker, &room);
        worker.muc_presence_cache.clear_connection();
        overflow_active_room(&mut worker, &room);
    });

    let output = String::from_utf8(captured.lock().expect("trace bytes").clone())
        .expect("UTF-8 trace output");
    assert_eq!(
        output
            .matches("xmpp MUC presence cache limit reached; quarantining room until a fresh join")
            .count(),
        2
    );
    for secret in [
        "tau-agent-1",
        "conference.example.org",
        "secret-nick",
        "secret-person",
        "private-resource",
    ] {
        assert!(!output.contains(secret), "warning leaked `{secret}`");
    }
}

/// A fresh join clears quarantine and mappings, while a connection reset also
/// resets the one-warning marker and all room state.
#[test]
fn muc_presence_cache_fresh_join_and_connection_lifecycle_purge_state() {
    let room = Jid::new("room@conference.example.org")
        .expect("jid")
        .to_bare();
    let mut cache = MucPresenceCache::default();
    for index in 0..=MAX_MUC_OCCUPANTS_PER_ROOM {
        let _ = cache.admit(
            full_jid(&format!("room@conference.example.org/nick-{index}")),
            full_jid(&format!("user-{index}@example.org/resource")),
        );
    }
    assert!(cache.is_quarantined(&room));
    assert!(cache.take_warning(&room));
    assert!(!cache.take_warning(&room));
    cache.begin_join(&room);
    assert!(!cache.is_quarantined(&room));
    assert_eq!(cache.total_len(), 0);
    assert!(!cache.take_warning(&room));
    cache.clear_connection();
    assert!(cache.take_warning(&room));
}

/// Warning suppression metadata is itself bounded, and once full it suppresses
/// later warnings until a new connection resets both contents and allocation.
#[test]
fn muc_presence_cache_warning_metadata_is_bounded_per_connection() {
    let mut cache = MucPresenceCache::default();
    for index in 0..MAX_WARNED_MUC_ROOMS {
        let room = Jid::new(&format!("room-{index}@conference.example.org"))
            .expect("jid")
            .to_bare();
        assert!(cache.take_warning(&room));
        cache.purge_room(&room);
    }
    let overflow_room = Jid::new("warning-overflow@conference.example.org")
        .expect("jid")
        .to_bare();
    assert!(!cache.take_warning(&overflow_room));
    let (warning_count, warning_capacity, suppressed) = cache.warning_state();
    assert_eq!(warning_count, MAX_WARNED_MUC_ROOMS);
    assert!(warning_capacity < MAX_WARNED_MUC_ROOMS * 2);
    assert!(suppressed);

    cache.clear_connection();
    assert_eq!(cache.warning_state(), (0, 0, false));
    assert!(cache.take_warning(&overflow_room));
}

/// MUC groupchat messages without real-JID proof fail closed by default so room
/// anonymity cannot silently bypass `allowed_jids`.
#[test]
fn muc_message_without_real_jid_is_not_routed() {
    let (tx, rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    worker.room_to_agent.insert(
        Jid::new("tau-agent-1@conference.example.org")
            .expect("jid")
            .to_bare(),
        agent_id("agent-1"),
    );
    worker.conversations.insert(
        agent_id("agent-1"),
        Conversation::Muc {
            room: Jid::new("tau-agent-1@conference.example.org")
                .expect("jid")
                .to_bare(),
            nick: "tau-self".to_owned(),
        },
    );
    activate_worker_agent(&mut worker, agent_id("agent-1"));
    let mut message =
        Message::groupchat(Jid::new("tau-agent-1@conference.example.org").expect("jid"))
            .with_body(Lang::new(), "hello".to_owned());
    message.from = Some(Jid::new("tau-agent-1@conference.example.org/alice").expect("jid"));
    worker.handle_stanza(message.into());
    assert!(rx.try_recv().is_err());
}

/// MUC room identity is deterministic from the globally unique Tau agent id and
/// does not expose a redundant session id in the room localpart.
#[test]
fn muc_room_identity_uses_only_stable_agent_id() {
    let (tx, _rx) = mpsc::channel();
    let worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let room = default_muc_room(&worker, &agent_id("agent-1"));
    assert_eq!(room.to_string(), "agent-1-4zqfxb1k@conference.example.org");
}

/// A configured room template receives session, agent, role, and role-group
/// identity and may deliberately omit the default hash when the operator
/// accepts the resulting collision policy.
#[test]
fn muc_room_template_can_use_identity_without_mandatory_randomness() {
    let mut cfg = cfg();
    cfg.muc.room_template =
        "{{session_id}}-{{agent_id}}-{{role}}-{{role_group}}-{{role_id}}-{{group_id}}".to_owned();
    let mut state = State::default();
    state
        .agent_roles
        .insert(agent_id("agent-1"), "engineer".to_owned());
    state
        .role_groups
        .insert("engineer".to_owned(), "engineering".to_owned());

    let localpart = room_localpart_for_registration(
        &state,
        &cfg,
        &"session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        &agent_id("agent-1"),
    )
    .expect("render")
    .expect("MUC room");

    assert_eq!(
        localpart,
        "session-1-agent-1-engineer-engineering-engineer-engineering"
    );
    assert!(!localpart.contains(&muc_room_disambiguator(&agent_id("agent-1"))));
}

/// Optional role/group presence flags let one valid template handle
/// registration before metadata catch-up without silently inventing role
/// identifiers.
#[test]
fn muc_room_template_exposes_missing_metadata_flags() {
    let mut cfg = cfg();
    cfg.muc.room_template = "{{#if role_present}}{{role}}{{else}}no-role{{/if}}-{{#if role_group_present}}{{role_group}}{{else}}no-group{{/if}}".to_owned();

    let localpart = room_localpart_for_registration(
        &State::default(),
        &cfg,
        &"session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        &agent_id("agent-1"),
    )
    .expect("render")
    .expect("MUC room");

    assert_eq!(localpart, "no-role-no-group");
}

/// The optional random helper matches agent-id template ergonomics while
/// keeping randomness entirely opt-in and outside the stable default room
/// policy.
#[test]
fn muc_room_template_supports_opt_in_random_alphanumeric() {
    let mut cfg = cfg();
    cfg.muc.room_template = "{{random_alphanumeric 12}}".to_owned();

    let localpart = room_localpart_for_registration(
        &State::default(),
        &cfg,
        &"session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        &agent_id("agent-1"),
    )
    .expect("render")
    .expect("MUC room");

    assert_eq!(localpart.len(), 12);
    assert!(localpart.chars().all(|ch| ch.is_ascii_alphanumeric()));
}

/// The random helper rejects ambiguous or unsafe argument shapes during
/// configuration rather than silently substituting a different entropy policy.
#[test]
fn muc_room_template_rejects_invalid_random_lengths() {
    for template in [
        "{{random_alphanumeric}}",
        "{{random_alphanumeric \"bad\"}}",
        "{{random_alphanumeric -1}}",
        "{{random_alphanumeric 0}}",
        "{{random_alphanumeric 65}}",
        "{{random_alphanumeric 4 5}}",
        "{{random_alphanumeric 4 ignored=5}}",
    ] {
        let error = validate_room_template(
            Some(template.to_owned()),
            "tau",
            Some(&Jid::new("conference.example.org").expect("JID").to_bare()),
        )
        .expect_err(template);
        assert!(error.contains("random_alphanumeric"), "{template}: {error}");
    }
}

/// Invalid Handlebars variables are extension configuration errors, rather than
/// deferred bridge startup failures or silent fallback to a different room.
#[test]
fn invalid_muc_room_template_is_reported_as_config_error() {
    let frames = run_protocol_messages(
        &[HarnessOutputMessage::Configure(configure_from_json(
            serde_json::json!({
                "jid": "tau@example.org",
                "password_secret": "xmpp_password",
                "allowed_jids": ["me@example.org"],
                "default_recipient": "me@example.org",
                "routing": { "mode": "muc" },
                "muc": {
                    "service": "conference.example.org",
                    "room_template": "{{unknown_identity}}"
                },
            }),
        ))],
        FakeBridge::new(),
    );

    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::ConfigError(error)
            if error.message.contains("muc.room_template")
                && error.message.contains("unknown_identity")
    )));
}

/// Metadata that is valid Tau identity text but invalid in an XMPP localpart
/// fails the tool before the bridge starts.
#[test]
fn actual_room_template_metadata_is_validated_before_bridge_start() {
    let (tx, rx) = mpsc::channel();
    let bridge = FakeBridge::new();
    let ext = Extension::new(bridge.clone(), tx);
    let mut cfg = cfg();
    cfg.muc.room_template = "{{role}}".to_owned();
    ext.apply_config(cfg).expect("apply config");
    let mut state = ext.state.lock().expect("lock");
    state.current_session_id = Some(
        "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
    );
    state
        .agent_roles
        .insert(agent_id("agent-1"), "bad@role".to_owned());
    drop(state);

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("result") else {
        panic!("emit")
    };
    assert!(matches!(*emit.event, Event::ToolErrorReported(_)));
    assert_eq!(*bridge.started.lock().expect("lock"), 0);
}

/// Replayed durable agent-role and reconstructed role-group metadata are cached
/// before registration so role-based room templates work after daemon resume.
#[test]
fn replayed_role_metadata_populates_muc_room_template() {
    let bridge = FakeBridge::new();
    bridge.set_ready(true);
    run_protocol_messages(
        &[
            HarnessOutputMessage::Configure(configure_from_json(serde_json::json!({
                "jid": "tau@example.org",
                "password_secret": "xmpp_password",
                "allowed_jids": ["me@example.org"],
                "default_recipient": "me@example.org",
                "routing": { "mode": "muc" },
                "muc": {
                    "service": "conference.example.org",
                    "room_template": "{{role}}-{{role_group}}-{{agent_id}}"
                },
            }))),
            session_started_message("session-1"),
            HarnessOutputMessage::deliver_replay(
                tau_proto::UnixMicros::new(1),
                Event::AgentStarted(tau_proto::AgentStarted {
                    creator: Some(tau_proto::AgentCreator::default()),

                    agent_id: agent_id("agent-1"),
                    parent_agent: None,
                    role: "engineer".to_owned(),
                    display_name: None,
                    metadata: Vec::new(),
                    ephemeral: false,
                }),
            ),
            HarnessOutputMessage::deliver_replay(
                tau_proto::UnixMicros::new(2),
                Event::HarnessRolesAvailable(tau_proto::HarnessRolesAvailable {
                    roles: vec![tau_proto::HarnessRoleInfo {
                        name: "engineer".to_owned(),
                        description: String::new(),
                        role_description: None,
                        details: None,
                    }],
                    groups: vec![tau_proto::HarnessRoleGroup {
                        name: "engineering".to_owned(),
                        roles: vec!["engineer".to_owned()],
                    }],
                    custom_prompts: Vec::new(),
                }),
            ),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-1",
                bool_args(true),
            ))),
        ],
        bridge.clone(),
    );

    assert_eq!(
        bridge.registrations.lock().expect("lock").last(),
        Some(&(
            agent_id("agent-1"),
            "engineer-engineering-agent-1@conference.example.org".to_owned()
        ))
    );
}

/// Full agent ids participate in the MUC room hash so long ids with identical
/// display prefixes cannot collapse onto one room and overwrite inbound
/// routing.
#[test]
fn muc_room_identity_hashes_full_long_agent_ids() {
    let (tx, _rx) = mpsc::channel();
    let worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let first = agent_id(&format!("{}{}", "a".repeat(48), "b".repeat(16)));
    let second = agent_id(&format!("{}{}", "a".repeat(48), "c".repeat(16)));

    let first_room = default_muc_room(&worker, &first);
    let second_room = default_muc_room(&worker, &second);

    assert_ne!(first_room, second_room);
    assert_ne!(
        muc_room_disambiguator(&first),
        muc_room_disambiguator(&second)
    );
    for (room, agent) in [(first_room, &first), (second_room, &second)] {
        assert_room_shape(&room, agent.as_ref());
    }
}

/// Agent ids that differ only by case remain distinct after XMPP JID parsing
/// and normalization because room identity hashes the raw Tau ids and encodes
/// the disambiguator as lowercase base32, not as raw localpart text.
#[test]
fn muc_room_identity_is_stable_across_xmpp_nodeprep_casefolding() {
    let (tx, _rx) = mpsc::channel();
    let worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let uppercase = default_muc_room(&worker, &agent_id("AgentA"));
    let lowercase = default_muc_room(&worker, &agent_id("agenta"));

    assert_eq!(
        uppercase.to_string(),
        "agenta-mj9z3t3v@conference.example.org"
    );
    assert_eq!(
        lowercase.to_string(),
        "agenta-vkqr78d6@conference.example.org"
    );
    assert_ne!(uppercase, lowercase);
}

/// The default retains the full generated Tau agent id, including its unique
/// generated suffix.
#[test]
fn muc_room_identity_keeps_full_generated_agent_id() {
    let (tx, _rx) = mpsc::channel();
    let worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());

    let room = default_muc_room(&worker, &agent_id("manager-Y3KG"));

    assert_eq!(
        room.to_string(),
        "manager-y3kg-bq7e2a4f@conference.example.org"
    );
}

/// If two generated identities ever collide onto the same normalized room JID,
/// registration must fail closed instead of overwriting inbound routing.
#[test]
fn muc_room_collision_does_not_overwrite_existing_routing() {
    let (tx, _rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let room = default_muc_room(&worker, &agent_id("agent-1"));
    worker
        .room_to_agent
        .insert(room.clone(), agent_id("agent-2"));

    let err = worker
        .ensure_muc_room_available(&room, &agent_id("agent-1"))
        .expect_err("collision rejected");

    assert!(err.contains("collision"));
    assert_eq!(worker.room_to_agent.get(&room), Some(&agent_id("agent-2")));
}

/// A rendered room that is already in a pending join for another agent must
/// also fail closed so registration races cannot claim the same room.
#[test]
fn muc_room_collision_does_not_overwrite_pending_join() {
    let (tx, _rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let room = default_muc_room(&worker, &agent_id("agent-1"));
    worker.pending_muc_joins.insert(
        agent_id("agent-2"),
        MucOccupant::new(room.clone(), "tau-other".to_owned()),
    );

    let err = worker
        .ensure_muc_room_available(&room, &agent_id("agent-1"))
        .expect_err("pending collision rejected");

    assert!(err.contains("collision"));
    assert_eq!(
        worker
            .pending_muc_joins
            .get(&agent_id("agent-2"))
            .map(|occupant| (&occupant.room, occupant.nick.as_str())),
        Some((&room, "tau-other"))
    );
}

/// Default room localparts remain XMPP-safe and bounded by the validated
/// agent-id limit, regardless of the unused legacy room prefix.
#[test]
fn muc_room_identity_is_bounded_and_xmpp_localpart_safe() {
    let (tx, _rx) = mpsc::channel();
    let mut cfg = cfg();
    cfg.muc.room_prefix = "x".repeat(48);
    let worker = WorkerState::new(cfg, tx, shutdown_signal(), test_authority());

    let room = default_muc_room(&worker, &agent_id(&"a".repeat(64))).to_string();
    let localpart = room
        .split_once('@')
        .map(|(localpart, _)| localpart)
        .expect("room localpart");

    assert_eq!(localpart.len(), 64 + 1 + 8);
    assert!(
        localpart
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
    );
}

/// Config validation applies domain-specific prefix fallbacks after
/// sanitization; an unsafe room prefix must not accidentally fall back to a
/// resource prefix.
#[test]
fn config_prefix_sanitization_uses_call_site_fallbacks() {
    let cfg = ExtConfig {
        jid: Some("tau@example.org".to_owned()),
        password_secret: Some("xmpp_password".to_owned()),
        allowed_jids: vec!["me@example.org".to_owned()],
        default_recipient: Some("me@example.org".to_owned()),
        routing: RoutingConfig {
            mode: Some("muc".to_owned()),
        },
        resource_prefix: Some("!@#".to_owned()),
        muc: MucConfigRaw {
            service: Some("conference.example.org".to_owned()),
            room_prefix: Some("!@#".to_owned()),
            ..Default::default()
        },
        ..Default::default()
    }
    .validate(&secrets(), None)
    .expect("valid config");

    assert_eq!(cfg.resource_prefix, DEFAULT_RESOURCE_PREFIX);
    assert_eq!(cfg.muc.room_prefix, DEFAULT_ROOM_PREFIX);
}

/// Worker-wide command/online operations are raced against shutdown so an
/// in-flight XMPP operation can be interrupted before cleanup is attempted.
#[tokio::test]
async fn worker_operations_are_cancelled_by_shutdown() {
    let shutdown = shutdown_signal();
    let shutdown_trigger = Arc::clone(&shutdown);

    let outcome = run_until_worker_shutdown(Arc::clone(&shutdown), async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        shutdown_trigger.request();
    });

    assert_eq!(outcome.await, WorkerRunOutcome::Shutdown);
}

/// Shutdown waiters wake from notification promptly instead of relying on the
/// removed polling interval; this prevents reintroducing tau-agent-t48g.
#[tokio::test]
async fn shutdown_signal_wakes_waiters_without_polling() {
    let shutdown = shutdown_signal();
    let waiter_shutdown = Arc::clone(&shutdown);
    let waiter = tokio::spawn(async move {
        waiter_shutdown.wait().await;
    });

    tokio::task::yield_now().await;
    shutdown.request();

    tokio::time::timeout(Duration::from_millis(25), waiter)
        .await
        .expect("shutdown signal should wake faster than the old polling interval")
        .expect("shutdown waiter task should not panic");
}

/// Best-effort helper operations also observe the shutdown signal directly so
/// lower-priority notices can be skipped once cleanup starts.
#[tokio::test]
async fn best_effort_helper_operations_are_cancelled_by_shutdown() {
    let (tx, _rx) = mpsc::channel();
    let shutdown = shutdown_signal();
    shutdown.request();
    let worker = WorkerState::new(cfg(), tx, Arc::clone(&shutdown), test_authority());

    let err = worker
        .until_shutdown(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        })
        .await
        .expect_err("shutdown cancels operation");

    assert!(err.contains("shutdown requested"));
}

/// Formal MUC invitations use XEP-0045 mediated invite payloads addressed to
/// the room so clients can present the room as a joinable conversation.
#[test]
fn muc_invite_message_contains_mediated_invite_payload() {
    let room = Jid::new("tau-r0123456789abcdef0123456789abcdef@conference.example.org")
        .expect("room")
        .to_bare();
    let recipient = Jid::new("me@example.org").expect("recipient");
    let message = muc_invite_message(room.clone(), recipient.clone(), "join this Tau room");

    assert_eq!(message.type_, MessageType::Normal);
    assert_eq!(message.to, Some(room.into()));
    let payload = message.payloads.first().expect("muc user payload").clone();
    let muc_user = MucUser::try_from(payload).expect("muc user");
    let invite = muc_user.invite.expect("invite");
    assert_eq!(invite.to, Some(recipient));
    assert_eq!(invite.reason.as_deref(), Some("join this Tau room"));
}

/// XMPP delivered identities must be stable for the same native composite,
/// separated by sender/conversation authority, and unique and bounded when the
/// stanza omits an id.
#[test]
fn inbound_message_ids_follow_native_composite_and_local_fallback_rules() {
    let (tx, _rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let mut native = Message::chat(Jid::new("tau@example.org").expect("jid"))
        .with_body(Lang::new(), "hello".to_owned());
    native.id = Some(xmpp_parsers::message::Id("native-1".to_owned()));

    let first = worker.inbound_message_id(&native, "alice@example.org", "room@example.org");
    let same = worker.inbound_message_id(&native, "alice@example.org", "room@example.org");
    let other_sender = worker.inbound_message_id(&native, "bob@example.org", "room@example.org");
    let other_conversation =
        worker.inbound_message_id(&native, "alice@example.org", "other@example.org");
    assert_eq!(first, same);
    assert_ne!(first, other_sender);
    assert_ne!(first, other_conversation);
    assert!(!first.as_str().contains("alice"));
    assert!(!first.as_str().contains("room"));

    native.id = None;
    let generated_one = worker.inbound_message_id(&native, "alice@example.org", "room@example.org");
    let generated_two = worker.inbound_message_id(&native, "alice@example.org", "room@example.org");
    assert_ne!(generated_one, generated_two);
    for id in [first, generated_one, generated_two] {
        assert!(id.as_str().len() <= 256);
        assert!(id.as_str().starts_with("xmpp-delivered:"));
    }
    let sender = xmpp_sender_ref("alice@example.org");
    assert_eq!(sender, xmpp_sender_ref("alice@example.org"));
    assert_ne!(sender, xmpp_sender_ref("bob@example.org"));
    assert!(sender.starts_with("xmpp-sender:"));
    assert!(!sender.contains("alice"));
    assert!(!sender.contains("example.org"));
}

/// Allowed MUC text with a cached real JID submits one transport-neutral report
/// whose live projection is identical on replay.
#[test]
fn allowed_muc_message_submits_replay_stable_report() {
    let (tx, rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let room = Jid::new("tau-agent-1@conference.example.org")
        .expect("jid")
        .to_bare();
    let occupant = Jid::new("tau-agent-1@conference.example.org/alice").expect("jid");
    worker
        .room_to_agent
        .insert(room.clone(), agent_id("agent-1"));
    retain_muc_identity(
        &mut worker,
        occupant.clone(),
        Jid::new("me@example.org/dino").expect("jid"),
    );
    worker.conversations.insert(
        agent_id("agent-1"),
        Conversation::Muc {
            room,
            nick: "tau-self".to_owned(),
        },
    );
    activate_worker_agent(&mut worker, agent_id("agent-1"));
    let mut message =
        Message::groupchat(Jid::new("tau-agent-1@conference.example.org").expect("jid"))
            .with_body(Lang::new(), "hello".to_owned());
    message.from = Some(occupant);
    worker.handle_stanza(message.into());
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    assert!(!emit.persist);
    let Event::MessageDeliveredReported(report) = *emit.event else {
        panic!("message.delivered_reported event")
    };
    assert_eq!(report.publisher_extension_id.as_str(), "std-xmpp");
    assert_eq!(report.agent_id.as_str(), "agent-1");
    assert_eq!(report.text, "hello");
    assert_eq!(report.sender.stable_id, xmpp_sender_ref("me@example.org"));
    assert_eq!(
        report.sender.sender_auth,
        Some(MessageSenderAuth::VerifiedAllowlisted)
    );
    assert_eq!(
        report
            .conversation
            .as_ref()
            .expect("conversation")
            .stable_id,
        "tau-agent-1@conference.example.org"
    );
    assert_delivered_live_replay_parity(report);
}

/// MUC messages with an unallowlisted real JID must be dropped even when they
/// arrive in a known room.
#[test]
fn muc_message_from_unallowed_real_jid_is_not_routed() {
    let (mut worker, rx, _room, occupant) = worker_with_muc_agent();
    retain_muc_identity(
        &mut worker,
        occupant.clone(),
        Jid::new("mallory@example.org").expect("jid"),
    );
    worker.handle_stanza(muc_message(occupant, "hello"));
    assert!(rx.try_recv().is_err());
}

/// Hidden-real-JID MUC messages remain fail-closed when trust in server-side
/// membership has not been explicitly enabled.
#[test]
fn muc_hidden_real_jid_without_trust_is_not_routed_even_when_expose_false() {
    let (mut worker, rx, _room, occupant) = worker_with_muc_agent();
    worker.cfg.muc.expose_real_jids = false;
    worker.cfg.muc.trust_muc_membership = false;
    worker.handle_stanza(muc_message(occupant, "hello"));
    assert!(rx.try_recv().is_err());
}

/// When the user explicitly accepts server-side room membership as the guard,
/// MUC messages without real-JID proof may route with occupant context.
#[test]
fn muc_hidden_real_jid_with_trust_routes() {
    let (mut worker, rx, _room, occupant) = worker_with_muc_agent();
    worker.cfg.muc.expose_real_jids = false;
    worker.cfg.muc.trust_muc_membership = true;
    worker.handle_stanza(muc_message(occupant, "hello"));
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    let Event::MessageDeliveredReported(report) = *emit.event else {
        panic!("message.delivered_reported event")
    };
    assert_eq!(report.text, "hello");
    assert_eq!(report.sender.display_name.as_deref(), Some("alice"));
    assert_eq!(
        report.sender.sender_auth,
        Some(MessageSenderAuth::TrustedMembership)
    );
}

/// Trusted-membership MUC reports retain the accepted occupant nickname as
/// descriptive metadata; generic projection owns visible escaping.
#[test]
fn muc_hidden_real_jid_occupant_label_is_descriptive_metadata() {
    let (mut worker, rx, room, _occupant) = worker_with_muc_agent();
    worker.cfg.muc.trust_muc_membership = true;
    let occupant = Jid::new("tau-agent-1@conference.example.org/alice] [xmpp direct").expect("jid");
    worker.handle_stanza(muc_message(occupant, "hello"));
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    let Event::MessageDeliveredReported(report) = *emit.event else {
        panic!("message.delivered_reported event")
    };
    assert_eq!(report.text, "hello");
    assert_eq!(
        report.sender.display_name.as_deref(),
        Some("alice] [xmpp direct")
    );
    assert_eq!(
        report.conversation.expect("conversation").stable_id,
        room.as_str()
    );
}

/// The bridge must suppress groupchat echoes from its own occupant nick so
/// agent replies do not come back as fresh delivered reports.
#[test]
fn muc_own_message_is_not_routed() {
    let (mut worker, rx, _room, _occupant) = worker_with_muc_agent();
    let own = Jid::new("tau-agent-1@conference.example.org/tau-self").expect("jid");
    retain_muc_identity(
        &mut worker,
        own.clone(),
        Jid::new("me@example.org/dino").expect("jid"),
    );
    worker.handle_stanza(muc_message(own, "echo"));
    assert!(rx.try_recv().is_err());
}

/// Oversized inbound MUC text is dropped before report submission to bound
/// external prompt amplification.
#[test]
fn oversized_muc_message_is_not_routed() {
    let (mut worker, rx, _room, occupant) = worker_with_muc_agent();
    worker.cfg.max_message_bytes = 4;
    retain_muc_identity(
        &mut worker,
        occupant.clone(),
        Jid::new("me@example.org/dino").expect("jid"),
    );
    worker.handle_stanza(muc_message(occupant, "hello"));
    assert!(rx.try_recv().is_err());
}

/// Delayed MUC history is ignored so joining or reconnecting to a room cannot
/// turn old backlog into fresh Tau prompts.
#[test]
fn delayed_muc_history_is_not_routed() {
    let (mut worker, rx, _room, occupant) = worker_with_muc_agent();
    retain_muc_identity(
        &mut worker,
        occupant.clone(),
        Jid::new("me@example.org/dino").expect("jid"),
    );
    worker.handle_stanza(delayed_muc_message(occupant, "old hello"));
    assert!(rx.try_recv().is_err());
}

/// Coming online after a reconnect clears cached MUC occupant real JIDs so
/// stale authorization evidence cannot survive a fresh stream.
#[test]
fn online_state_clears_muc_real_jid_cache() {
    let (mut worker, _rx, _room, occupant) = worker_with_muc_agent();
    retain_muc_identity(
        &mut worker,
        occupant,
        Jid::new("me@example.org/dino").expect("jid"),
    );
    worker.apply_online_state(Jid::new("tau@example.org/new").expect("jid"));
    assert_eq!(worker.muc_presence_cache.total_len(), 0);
}

/// Direct-resource reconnect handling updates the stored full JID and returns a
/// notification work item so the human recipient can learn the new address.
#[test]
fn online_state_updates_direct_resource_full_jid() {
    let (tx, _rx) = mpsc::channel();
    let mut cfg = cfg();
    cfg.routing_mode = RoutingMode::DirectResource;
    let mut worker = WorkerState::new(cfg, tx, shutdown_signal(), test_authority());
    worker.conversations.insert(
        agent_id("agent-1"),
        Conversation::Direct {
            full_jid: Jid::new("tau@example.org/old").expect("jid"),
        },
    );
    let updates = worker.apply_online_state(Jid::new("tau@example.org/new").expect("jid"));
    assert_eq!(
        updates,
        vec![(
            agent_id("agent-1"),
            Jid::new("tau@example.org/new").expect("jid")
        )]
    );
    let Some(Conversation::Direct { full_jid }) = worker.conversations.get(&agent_id("agent-1"))
    else {
        panic!("direct conversation")
    };
    assert_eq!(full_jid, &Jid::new("tau@example.org/new").expect("jid"));
}

/// Reconnect handling computes the exact MUC room/nick pairs that need rejoin
/// stanzas while ignoring direct-resource conversations.
#[test]
fn muc_rooms_to_rejoin_lists_only_muc_conversations() {
    let (mut worker, _rx, room, _occupant) = worker_with_muc_agent();
    worker.conversations.insert(
        agent_id("agent-2"),
        Conversation::Direct {
            full_jid: Jid::new("tau@example.org/direct").expect("jid"),
        },
    );
    assert_eq!(
        worker.muc_rooms_to_rejoin(),
        vec![(room, "tau-self".to_owned())]
    );
}

/// Unavailable MUC presence invalidates the occupant real-JID cache so a later
/// nick reuse cannot inherit the previous occupant's authorization.
#[test]
fn muc_unavailable_presence_invalidates_real_jid_cache() {
    let (mut worker, rx, _room, occupant) = worker_with_muc_agent();
    retain_muc_identity(
        &mut worker,
        occupant.clone(),
        Jid::new("me@example.org/dino").expect("jid"),
    );
    let mut presence = Presence::unavailable();
    presence.from = Some(occupant.clone());
    worker.handle_stanza(presence.into());
    worker.handle_stanza(muc_message(occupant, "hello"));
    assert!(rx.try_recv().is_err());
}

/// Direct-resource fallback accepts only allowed senders whose stanza `to`
/// exactly matches the current server-bound full JID.
#[test]
fn direct_message_requires_exact_bound_full_jid() {
    let (tx, rx) = mpsc::channel();
    let mut cfg = cfg();
    cfg.routing_mode = RoutingMode::DirectResource;
    let mut worker = WorkerState::new(cfg, tx, shutdown_signal(), test_authority());
    let bound = Jid::new("tau@example.org/tau-resource").expect("jid");
    worker.bound_jid = Some(bound.clone());
    worker.conversations.insert(
        agent_id("agent-1"),
        Conversation::Direct {
            full_jid: bound.clone(),
        },
    );
    activate_worker_agent(&mut worker, agent_id("agent-1"));

    let mut wrong_to = Message::chat(bound.clone()).with_body(Lang::new(), "hello".to_owned());
    wrong_to.from = Some(Jid::new("me@example.org/dino").expect("jid"));
    wrong_to.to = Some(Jid::new("tau@example.org/other").expect("jid"));
    worker.handle_stanza(wrong_to.into());
    assert!(rx.try_recv().is_err());

    let mut ok = Message::chat(bound.clone()).with_body(Lang::new(), "hello".to_owned());
    ok.from = Some(Jid::new("me@example.org/dino").expect("jid"));
    ok.to = Some(bound);
    worker.handle_stanza(ok.into());
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    let Event::MessageDeliveredReported(report) = *emit.event else {
        panic!("message.delivered_reported event")
    };
    assert_eq!(report.text, "hello");
    assert_eq!(report.sender.stable_id, xmpp_sender_ref("me@example.org"));
    assert_eq!(
        report.sender.sender_auth,
        Some(MessageSenderAuth::VerifiedAllowlisted)
    );
    assert_eq!(
        report.conversation.expect("conversation").stable_id,
        "me@example.org"
    );
}

/// The final inbound authority check rejects a stanza when revocation occurs
/// after route selection but immediately before publication.
#[test]
fn inbound_publication_revalidates_exact_lease_at_last_local_point() {
    let (tx, rx) = mpsc::channel();
    let mut config = cfg();
    config.routing_mode = RoutingMode::DirectResource;
    let authority = test_authority();
    let mut worker = WorkerState::new(config, tx, shutdown_signal(), Arc::clone(&authority));
    let agent = agent_id("agent-1");
    let bound = Jid::new("tau@example.org/tau-resource").expect("jid");
    worker.bound_jid = Some(bound.clone());
    worker.conversations.insert(
        agent.clone(),
        Conversation::Direct {
            full_jid: bound.clone(),
        },
    );
    let lease = activate_worker_agent(&mut worker, agent.clone());
    worker.before_inbound_publication = Some(Box::new(move || {
        assert!(authority.revoke(&agent, lease));
    }));
    let mut message = Message::chat(bound.clone()).with_body(Lang::new(), "hello".to_owned());
    message.from = Some(Jid::new("me@example.org/dino").expect("jid"));
    message.to = Some(bound);

    assert_eq!(
        worker.handle_stanza(message.into()),
        WorkerControl::Continue
    );
    assert!(rx.try_recv().is_err());
}

/// A failed accepted ingress report requests shutdown in the real worker path,
/// and a later queued stanza cannot route or mutate worker state.
#[test]
fn worker_report_failure_stops_before_later_stanza() {
    let (tx, rx) = mpsc::channel();
    drop(rx);
    let mut config = cfg();
    config.routing_mode = RoutingMode::DirectResource;
    let shutdown = shutdown_signal();
    let mut worker = WorkerState::new(config, tx, Arc::clone(&shutdown), test_authority());
    let bound = Jid::new("tau@example.org/tau-resource").expect("jid");
    worker.bound_jid = Some(bound.clone());
    worker.conversations.insert(
        agent_id("agent-1"),
        Conversation::Direct {
            full_jid: bound.clone(),
        },
    );
    activate_worker_agent(&mut worker, agent_id("agent-1"));
    let message = || {
        let mut message = Message::chat(bound.clone()).with_body(Lang::new(), "hello".to_owned());
        message.from = Some(Jid::new("me@example.org/dino").expect("jid"));
        message.to = Some(bound.clone());
        Stanza::from(message)
    };

    assert_eq!(worker.handle_stanza(message()), WorkerControl::Stop);
    assert!(shutdown.is_requested());
    let ordinal = worker.next_local_message_id;
    assert_eq!(worker.handle_stanza(message()), WorkerControl::Stop);
    assert_eq!(worker.next_local_message_id, ordinal);
}

/// Both nested worker readers propagate mandatory-report shutdown instead of
/// consuming another readiness or MUC event.
#[test]
fn nested_worker_readers_propagate_report_failure() {
    for context in ["online state", "muc join"] {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let mut config = cfg();
        config.routing_mode = RoutingMode::DirectResource;
        let shutdown = shutdown_signal();
        let mut worker = WorkerState::new(config, tx, shutdown, test_authority());
        let bound = Jid::new("tau@example.org/tau-resource").expect("jid");
        worker.bound_jid = Some(bound.clone());
        worker.conversations.insert(
            agent_id("agent-1"),
            Conversation::Direct {
                full_jid: bound.clone(),
            },
        );
        activate_worker_agent(&mut worker, agent_id("agent-1"));
        let mut message = Message::chat(bound.clone()).with_body(Lang::new(), "hello".to_owned());
        message.from = Some(Jid::new("me@example.org/dino").expect("jid"));
        message.to = Some(bound);
        assert_eq!(
            worker
                .handle_nested_stanza(message.into(), context)
                .expect_err("mandatory failure stops nested reader"),
            format!("xmpp worker stopped while waiting for {context}")
        );
    }
}

/// Direct-resource fallback refuses a second registration because one bound JID
/// cannot provide unambiguous one-to-one inbound routing for multiple agents.
#[tokio::test]
async fn direct_registration_rejects_second_agent() {
    let (tx, _rx) = mpsc::channel();
    let mut cfg = cfg();
    cfg.routing_mode = RoutingMode::DirectResource;
    let mut worker = WorkerState::new(cfg, tx, shutdown_signal(), test_authority());
    worker.bound_jid = Some(Jid::new("tau@example.org/tau-resource").expect("jid"));
    worker.conversations.insert(
        agent_id("agent-1"),
        Conversation::Direct {
            full_jid: Jid::new("tau@example.org/tau-resource").expect("jid"),
        },
    );
    let mut client = Client::new(
        Jid::new("tau@example.org/tau-resource").expect("jid"),
        "unused".to_owned(),
    );
    let err = worker
        .register_agent(agent_id("agent-2"), None, &mut client)
        .await
        .expect_err("second direct registration rejected");
    assert!(err.contains("only one registered agent"));
    assert!(err.contains("routing.mode `muc`"));
}

/// Removing a post-join MUC registration returns the tracked room and nick used
/// for unavailable leave presence and clears inbound routing maps.
#[test]
fn removing_muc_conversation_tracks_leave_and_clears_routing() {
    let (tx, _rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx, shutdown_signal(), test_authority());
    let agent = agent_id("agent-1");
    let room = Jid::new("tau-agent-1@conference.example.org")
        .expect("jid")
        .to_bare();
    worker.room_to_agent.insert(room.clone(), agent.clone());
    worker.conversations.insert(
        agent.clone(),
        Conversation::Muc {
            room: room.clone(),
            nick: "tau-self".to_owned(),
        },
    );
    overflow_active_room(&mut worker, &room);
    assert!(worker.muc_presence_cache.is_quarantined(&room));

    let removed = worker
        .remove_conversation(&agent)
        .expect("removed conversation");

    assert!(!worker.conversations.contains_key(&agent));
    assert!(worker.room_to_agent.values().all(|mapped| mapped != &agent));
    assert!(!worker.muc_presence_cache.is_quarantined(&room));
    let Conversation::Muc { room, nick } = removed else {
        panic!("muc conversation")
    };
    let presence = leave_presence(&room, &nick).expect("leave presence");
    assert_eq!(presence.type_, PresenceType::Unavailable);
    assert_eq!(
        presence.to,
        Some(Jid::new("tau-agent-1@conference.example.org/tau-self").expect("jid"))
    );
}
