use std::io::BufRead;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};

use tau_proto::{HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, ToolStarted};

use super::*;

#[derive(Clone, Default)]
struct SharedWriter {
    /// Shared byte buffer written by the tau-client writer thread.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("lock shared writer").clone()
    }
}

impl std::io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().expect("lock shared writer").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FakeClient {
    sent: Mutex<Vec<(i64, String)>>,
    update_batches: Mutex<Vec<Vec<TgUpdate>>>,
    poll_timeouts: Mutex<Vec<u64>>,
    webhook_info: Mutex<Result<TgWebhookInfo, String>>,
    send_error: Mutex<Option<String>>,
}

impl FakeClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            update_batches: Mutex::new(Vec::new()),
            poll_timeouts: Mutex::new(Vec::new()),
            webhook_info: Mutex::new(Ok(TgWebhookInfo::default())),
            send_error: Mutex::new(None),
        })
    }

    fn with_updates(update_batches: Vec<Vec<TgUpdate>>) -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            update_batches: Mutex::new(update_batches),
            poll_timeouts: Mutex::new(Vec::new()),
            webhook_info: Mutex::new(Ok(TgWebhookInfo::default())),
            send_error: Mutex::new(None),
        })
    }

    fn with_webhook_info(info: Result<TgWebhookInfo, String>) -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            update_batches: Mutex::new(Vec::new()),
            poll_timeouts: Mutex::new(Vec::new()),
            webhook_info: Mutex::new(info),
            send_error: Mutex::new(None),
        })
    }

    fn fail_sends(&self, message: &str) {
        *self.send_error.lock().expect("lock") = Some(message.to_owned());
    }
}

impl TelegramClient for FakeClient {
    fn get_webhook_info(&self, _cfg: &RuntimeConfig) -> Result<TgWebhookInfo, String> {
        self.webhook_info.lock().expect("lock").clone()
    }

    fn get_updates(
        &self,
        _cfg: &RuntimeConfig,
        _offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, String> {
        self.poll_timeouts
            .lock()
            .expect("lock")
            .push(_cfg.poll_timeout_seconds);
        let mut batches = self.update_batches.lock().expect("lock");
        if batches.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(batches.remove(0))
        }
    }

    fn send_message(&self, _cfg: &RuntimeConfig, chat_id: i64, text: &str) -> Result<(), String> {
        if let Some(message) = self.send_error.lock().expect("lock").clone() {
            return Err(message);
        }
        self.sent
            .lock()
            .expect("lock")
            .push((chat_id, text.to_owned()));
        Ok(())
    }
}

struct SlowPollClient;

impl TelegramClient for SlowPollClient {
    fn get_webhook_info(&self, _cfg: &RuntimeConfig) -> Result<TgWebhookInfo, String> {
        Ok(TgWebhookInfo::default())
    }

    fn get_updates(
        &self,
        _cfg: &RuntimeConfig,
        _offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, String> {
        std::thread::sleep(Duration::from_secs(2));
        Ok(Vec::new())
    }

    fn send_message(&self, _cfg: &RuntimeConfig, _chat_id: i64, _text: &str) -> Result<(), String> {
        Ok(())
    }
}

struct ControlledPollClient {
    first_response: Mutex<Option<Result<Vec<TgUpdate>, String>>>,
    response_ready: Condvar,
    called: Mutex<usize>,
    called_ready: Condvar,
}

impl ControlledPollClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            first_response: Mutex::new(None),
            response_ready: Condvar::new(),
            called: Mutex::new(0),
            called_ready: Condvar::new(),
        })
    }

    fn wait_for_call(&self) {
        self.wait_for_call_count(1);
    }

    fn wait_for_call_count(&self, expected: usize) {
        let called = self.called.lock().expect("lock");
        let (called, _timeout) = self
            .called_ready
            .wait_timeout_while(called, Duration::from_secs(1), |called| *called < expected)
            .expect("wait");
        assert!(
            *called >= expected,
            "poller issued {called} getUpdates calls, expected {expected}"
        );
    }

    fn release_first_response(&self, updates: Vec<TgUpdate>) {
        self.release_response(Ok(updates));
    }

    fn release_error(&self, message: &str) {
        self.release_response(Err(message.to_owned()));
    }

    fn release_response(&self, response: Result<Vec<TgUpdate>, String>) {
        *self.first_response.lock().expect("lock") = Some(response);
        self.response_ready.notify_all();
    }
}

impl TelegramClient for ControlledPollClient {
    fn get_webhook_info(&self, _cfg: &RuntimeConfig) -> Result<TgWebhookInfo, String> {
        Ok(TgWebhookInfo::default())
    }

    fn get_updates(
        &self,
        _cfg: &RuntimeConfig,
        _offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, String> {
        {
            let mut called = self.called.lock().expect("lock");
            *called += 1;
            self.called_ready.notify_all();
        }
        let response = self.first_response.lock().expect("lock");
        let mut response = self
            .response_ready
            .wait_while(response, |response| response.is_none())
            .expect("wait");
        response.take().unwrap_or_else(|| Ok(Vec::new()))
    }

    fn send_message(&self, _cfg: &RuntimeConfig, _chat_id: i64, _text: &str) -> Result<(), String> {
        Ok(())
    }
}

fn cfg() -> RuntimeConfig {
    RuntimeConfig {
        bot_token: "token".to_owned(),
        allowed_user_ids: [123].into_iter().collect(),
        configured_chat_id: Some(123),
        api_base: DEFAULT_API_BASE.to_owned(),
        poll_timeout_seconds: 1,
    }
}

fn temp_ext_root() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tau-ext-telegram-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create temp state dir");
    dir
}

fn temp_state_dir() -> std::path::PathBuf {
    temp_ext_root().join("std-telegram")
}

fn agent_id(text: &str) -> AgentId {
    AgentId::parse(text).expect("agent id")
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

fn message_args(value: &str) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("message".to_owned()),
        CborValue::Text(value.to_owned()),
    )])
}

fn gateway_mode(socket_path: std::path::PathBuf) -> BridgeMode {
    BridgeMode::GatewayClient(GatewayClientConfig { socket_path })
}

fn extension() -> (
    Extension,
    mpsc::Receiver<HarnessInputMessage>,
    Arc<FakeClient>,
) {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    (ext, rx, client)
}

fn process_update(ext: &Extension, update: TgUpdate) {
    let config_generation = ext.state.lock().config_generation;
    ext.process_update_for_generation(update, config_generation);
}

fn expect_tool_finished(rx: &mpsc::Receiver<HarnessInputMessage>) {
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
}

/// A successful Telegram send must publish `message.sent` before its ordinary
/// terminal tool result on the serialized extension output.
fn expect_successful_send(rx: &mpsc::Receiver<HarnessInputMessage>) -> MessageSent {
    let _progress = rx.recv().expect("progress");
    let message = rx.recv().expect("message.sent");
    let HarnessInputMessage::Emit(emit) = message else {
        panic!("emit")
    };
    let Event::MessageSent(fact) = *emit.event else {
        panic!("message.sent fact")
    };
    let result = rx.recv().expect("tool result");
    let HarnessInputMessage::Emit(emit) = result else {
        panic!("emit")
    };
    assert!(matches!(*emit.event, Event::ToolResult(_)));
    fact
}

fn expect_tool_error(rx: &mpsc::Receiver<HarnessInputMessage>) -> String {
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolError(error) = *emit.event else {
        panic!("tool error")
    };
    error.message
}

fn expect_notice(rx: &mpsc::Receiver<HarnessInputMessage>) -> HarnessNotice {
    let msg = rx.recv().expect("notice");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::HarnessNotice(notice) = *emit.event else {
        panic!("notice")
    };
    notice
}

fn expect_delivered(rx: &mpsc::Receiver<HarnessInputMessage>) -> MessageDelivered {
    let msg = rx.recv().expect("prompt");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::MessageDelivered(fact) = *emit.event else {
        panic!("message.delivered fact")
    };
    fact
}

/// Stamp a bridge-produced delivered fact as the harness would, then prove its
/// projection is identical after a serde round trip.
fn assert_delivered_live_replay_parity(mut fact: MessageDelivered) {
    fact.publisher_extension_id = MessagePublisherId::new("std-telegram");
    let live = Event::MessageDelivered(fact);
    let encoded = serde_json::to_value(&live).expect("encode fact");
    let replay: Event = serde_json::from_value(encoded).expect("decode replay fact");
    assert_eq!(
        tau_proto::project_message_fact(&live),
        tau_proto::project_message_fact(&replay)
    );
}

/// Telegram prompt references remain deterministic and bounded without exposing
/// native chat, update, or user identifiers.
#[test]
fn telegram_prompt_references_are_opaque_and_domain_separated() {
    let message = telegram_message_ref("native-chat", "native-update");
    assert_eq!(
        message,
        telegram_message_ref("native-chat", "native-update")
    );
    assert_ne!(message, telegram_message_ref("other-chat", "native-update"));
    assert_ne!(message, telegram_message_ref("native-chat", "other-update"));
    assert!(message.as_str().starts_with("telegram-message:"));
    assert!(message.as_str().len() <= 256);
    assert!(!message.as_str().contains("native"));

    let sender = telegram_sender_ref("42");
    assert_eq!(sender, telegram_sender_ref("42"));
    assert_ne!(sender, telegram_sender_ref("43"));
    assert!(sender.starts_with("telegram-sender:"));
    assert_eq!(sender.len(), "telegram-sender:".len() + 64);
}

/// Telegram bridge tools are disabled by default because each role must make an
/// explicit policy choice before exposing the external chat bridge to a model.
#[test]
fn telegram_tools_are_role_opt_in() {
    assert!(!register_tool_spec().enabled_by_default);
    assert!(!send_tool_spec().enabled_by_default);
}

/// Telegram bridge tools expose group and tag metadata so role policy can
/// enable the bridge broadly or select registration/sending capabilities
/// separately.
#[test]
fn telegram_tools_have_group_and_tags() {
    assert_eq!(telegram_tool_group().name.as_str(), TOOL_GROUP_NAME);

    let register = register_tool_spec();
    assert!(
        register
            .tags
            .iter()
            .any(|tag| tag.as_str() == REGISTER_TOOL_TAG)
    );

    let send = send_tool_spec();
    assert!(send.tags.iter().any(|tag| tag.as_str() == SEND_TOOL_TAG));
}

/// Telegram uses only the generic configured SDK prefix and never derives names
/// from the operational instance key.
#[test]
fn telegram_uses_generic_tool_prefix() {
    let scope = tau_client::ToolNameScope::from_configure(&tau_proto::Configure {
        tool_prefix: Some(tau_proto::ToolNamePrefix::parse("work").expect("prefix")),
        config: CborValue::Null,
        instance_name: tau_proto::ExtensionName::new("arbitrary-instance"),
        state_dir: None,
        secrets: BTreeMap::new(),
    });
    let names = ToolNames::from_scope(&scope).expect("scoped names");
    assert_eq!(names.register.as_str(), "work_telegram_register");
    assert_eq!(names.send.as_str(), "work_telegram_send");
    assert_eq!(names.group.as_str(), "work_telegram");
}

/// Provider-owned repair examples must stay schema-valid as bridge tool
/// argument shapes evolve.
#[test]
fn telegram_tool_examples_are_schema_valid() {
    for spec in [register_tool_spec(), send_tool_spec()] {
        tau_core::validate_tool_examples(&spec)
            .unwrap_or_else(|error| panic!("invalid examples for {}: {error}", spec.name));
    }
}

/// Enabled config must name a non-empty token secret and a non-empty allowlist;
/// otherwise the extension cannot safely decide who may use the bot.
#[test]
fn config_rejects_missing_token_or_empty_allowlist() {
    let err = ExtConfig::default()
        .validate(&BTreeMap::new())
        .expect_err("missing token secret");
    assert!(err.contains("bot_token_secret"));

    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    let err = ExtConfig {
        bot_token_secret: Some("bot".to_owned()),
        ..Default::default()
    }
    .validate(&secrets)
    .expect_err("empty allowlist");
    assert!(err.contains("allowed_user_ids"));
}

/// Gateway-client mode deliberately does not read a bot token or require a
/// Telegram allowlist in the sidecar; those belong to the standalone gateway
/// process that owns polling.
#[test]
fn gateway_client_config_requires_only_socket_path() {
    let err = ExtConfig {
        mode: ExtMode::GatewayClient,
        ..ExtConfig::default()
    }
    .validate(&BTreeMap::new())
    .expect_err("missing socket should fail");
    assert!(err.contains("gateway_socket_path"));

    let mode = ExtConfig {
        mode: ExtMode::GatewayClient,
        gateway_socket_path: Some(PathBuf::from("/tmp/tau-telegram-test.sock")),
        ..ExtConfig::default()
    }
    .validate(&BTreeMap::new())
    .expect("gateway client config");
    assert!(matches!(mode, BridgeMode::GatewayClient(_)));
}

/// In gateway-client mode the sidecar must not touch Telegram polling APIs.
/// Registration goes to the local gateway socket, and any queued inbound
/// delivery is published locally as a direct `message.delivered` fact.
#[test]
fn gateway_client_registers_without_polling_and_submits_delivery() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let seen_requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen_requests_thread = Arc::clone(&seen_requests);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = std::io::BufReader::new(reader);
        for index in 0..3 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("gateway request JSON");
            seen_requests_thread.lock().expect("requests").push(request);
            let response = match index {
                0 => serde_json::json!({
                    "protocol_version": 3,
                    "ok": true,
                    "gateway_generation": "test",
                    "reannounce_required": true,
                    "deliveries": [],
                }),
                1 => serde_json::json!({
                    "protocol_version": 3,
                    "ok": true,
                    "deliveries": [{
                        "request_id": "telegram-1",
                        "session_id": "s1",
                        "agent_id": "agent-1",
                        "message_id": "telegram:10:99",
                        "sender_id": "42",
                        "source": "alice",
                        "conversation_id": "10",
                        "text": "hello"
                    }],
                }),
                _ => serde_json::json!({
                    "protocol_version": 3,
                    "ok": true,
                    "deliveries": [],
                }),
            };
            writeln!(stream, "{response}").expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    {
        let mut state = ext.state.lock();
        state.current_session_id = Some("s1".into());
    }
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));

    let _progress = rx.recv().expect("progress");
    let delivered = expect_delivered(&rx);
    let _result = rx.recv().expect("tool result");
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    let sent = expect_successful_send(&rx);
    server.join().expect("fake gateway thread");

    assert_eq!(delivered.agent_id.as_str(), "agent-1");
    assert_eq!(delivered.text, "hello");
    assert_eq!(
        delivered.message_id,
        telegram_message_ref("10", "telegram:10:99")
    );
    assert_eq!(delivered.sender.stable_id, telegram_sender_ref("42"));
    assert_eq!(
        delivered.sender.sender_auth,
        Some(MessageSenderAuth::VerifiedAllowlisted)
    );
    assert_eq!(sent.text, "reply");
    assert!(client.poll_timeouts.lock().expect("polls").is_empty());
    let requests = seen_requests.lock().expect("requests");
    assert_eq!(requests[0]["kind"], "hello");
    assert_eq!(requests[1]["kind"], "register_agent");
    assert_eq!(requests[1]["session_id"], "s1");
    assert_eq!(requests[1]["agent_id"], "agent-1");
    assert_eq!(requests[2]["kind"], "send_message");
    assert_eq!(requests[2]["session_id"], "s1");
    assert_eq!(requests[2]["agent_id"], "agent-1");
    assert_eq!(requests[2]["message"], "reply");
    assert!(client.sent.lock().expect("sent").is_empty());
}

/// In gateway-client mode `telegram_send` must forward only message text plus
/// local session/agent identity to the gateway, leaving Telegram destination
/// selection entirely inside the gateway.
#[test]
fn gateway_client_send_forwards_registered_agent_to_gateway() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let seen_requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen_requests_thread = Arc::clone(&seen_requests);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = std::io::BufReader::new(reader);
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("gateway request JSON");
            seen_requests_thread.lock().expect("requests").push(request);
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "protocol_version": 3,
                    "ok": true,
                    "deliveries": [],
                })
            )
            .expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    {
        let mut state = ext.state.lock();
        state.current_session_id = Some("s1".into());
        state.registered_agents.insert(agent_id("agent-1"));
    }

    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    let sent = expect_successful_send(&rx);
    server.join().expect("fake gateway thread");

    let requests = seen_requests.lock().expect("requests");
    assert_eq!(requests[0]["kind"], "hello");
    assert_eq!(requests[1]["kind"], "send_message");
    assert_eq!(requests[1]["session_id"], "s1");
    assert_eq!(requests[1]["agent_id"], "agent-1");
    assert_eq!(requests[1]["message"], "reply");
    assert!(requests[1].get("chat_id").is_none());
    assert!(client.sent.lock().expect("sent").is_empty());
    assert_eq!(sent.agent_id.as_str(), "agent-1");
    assert_eq!(sent.text, "reply");
}

/// A gateway-declared send failure must return only a tool error and must not
/// claim remote success with `message.sent`.
#[test]
fn gateway_client_send_failure_does_not_publish_sent_fact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = std::io::BufReader::new(reader);
        for index in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            let response = if index == 0 {
                serde_json::json!({
                    "protocol_version": 3,
                    "ok": true,
                    "deliveries": [],
                })
            } else {
                serde_json::json!({
                    "protocol_version": 3,
                    "ok": false,
                    "error": "gateway send failed",
                    "keep_connection": true,
                })
            };
            writeln!(stream, "{response}").expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    {
        let mut state = ext.state.lock();
        state.current_session_id = Some("s1".into());
        state.registered_agents.insert(agent_id("agent-1"));
    }

    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    assert!(expect_tool_error(&rx).contains("gateway send failed"));
    assert!(
        rx.try_recv().is_err(),
        "unexpected message.sent after failure"
    );
    server.join().expect("fake gateway thread");
}

/// Registering before the sidecar has observed `session.started` must fail
/// locally and must not create an incomplete gateway route.
#[test]
fn gateway_client_register_before_session_started_does_not_announce() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let seen_requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen_requests_thread = Arc::clone(&seen_requests);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = std::io::BufReader::new(reader);
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            if line.trim().is_empty() {
                break;
            }
            seen_requests_thread
                .lock()
                .expect("requests")
                .push(serde_json::from_str(&line).expect("gateway request JSON"));
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "protocol_version": 3,
                    "ok": true,
                    "deliveries": [],
                })
            )
            .expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let message = expect_tool_error(&rx);
    assert!(message.contains("session.started"), "{message}");
    drop(ext);
    server.join().expect("fake gateway thread");

    let requests = seen_requests.lock().expect("requests");
    assert_eq!(requests[0]["kind"], "hello");
    assert!(
        requests
            .iter()
            .all(|request| request["kind"] != "register_agent"),
        "{requests:?}"
    );
}

/// Gateway deliveries are only accepted for the current session and a currently
/// registered local agent; stale gateway records cannot be published after
/// local state has failed closed or unregistered.
#[test]
fn gateway_delivery_requires_live_local_registration() {
    let (tx, rx) = mpsc::channel();
    let state = SharedState::new();
    {
        let mut state = state.lock();
        state.current_session_id = Some("s1".into());
    }
    emit_gateway_deliveries(
        &state,
        &Output::Channel(tx.clone()),
        vec![GatewayMessageDelivery {
            request_id: "telegram-1".to_owned(),
            session_id: "s1".to_owned(),
            agent_id: "agent-1".to_owned(),
            message_id: "telegram:1:1".to_owned(),
            sender_id: "7".to_owned(),
            source: "alice".to_owned(),
            conversation_id: "1".to_owned(),
            text: "hello".to_owned(),
        }],
    );
    assert!(rx.try_recv().is_err());

    state.lock().registered_agents.insert(agent_id("agent-1"));
    emit_gateway_deliveries(
        &state,
        &Output::Channel(tx),
        vec![GatewayMessageDelivery {
            request_id: "telegram-2".to_owned(),
            session_id: "s1".to_owned(),
            agent_id: "agent-1".to_owned(),
            message_id: "telegram:1:2".to_owned(),
            sender_id: "7".to_owned(),
            source: "alice".to_owned(),
            conversation_id: "1".to_owned(),
            text: "hello again".to_owned(),
        }],
    );
    let delivered = expect_delivered(&rx);
    assert_eq!(delivered.text, "hello again");
    assert_eq!(delivered.sender.stable_id, telegram_sender_ref("7"));
    assert_eq!(delivered.conversation.expect("conversation").stable_id, "1");
}

/// A heartbeat failure from a stale gateway connection must not clear
/// registrations that belong to a newer active gateway or mode.
#[test]
fn stale_gateway_heartbeat_failure_does_not_clear_new_registration_state() {
    let gateway_cell = Mutex::new(None);
    let state = SharedState::new();
    let old_gateway = Arc::new(GatewayClient::new(GatewayClientConfig {
        socket_path: PathBuf::from("/tmp/old-gateway.sock"),
    }));
    let new_gateway = Arc::new(GatewayClient::new(GatewayClientConfig {
        socket_path: PathBuf::from("/tmp/new-gateway.sock"),
    }));
    *gateway_cell.lock().expect("gateway lock") = Some(Arc::clone(&new_gateway));
    {
        let mut state = state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state.selected_agent_by_chat.insert(10, agent_id("agent-1"));
    }

    assert!(!fail_gateway_client_if_current(
        &gateway_cell,
        &state,
        &old_gateway
    ));
    assert!(
        state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-1"))
    );
    assert!(gateway_cell.lock().expect("gateway lock").is_some());

    assert!(fail_gateway_client_if_current(
        &gateway_cell,
        &state,
        &new_gateway
    ));
    assert!(state.lock().registered_agents.is_empty());
    assert!(gateway_cell.lock().expect("gateway lock").is_none());
}

/// Clearing malformed configuration must send `goodbye` to release gateway
/// leases instead of silently dropping local state.
#[test]
fn gateway_client_config_error_sends_goodbye() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let seen_requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen_requests_thread = Arc::clone(&seen_requests);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = std::io::BufReader::new(reader);
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            seen_requests_thread
                .lock()
                .expect("requests")
                .push(serde_json::from_str(&line).expect("gateway request JSON"));
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "protocol_version": 3,
                    "ok": true,
                    "deliveries": [],
                })
            )
            .expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    ext.clear_config_after_error();
    server.join().expect("fake gateway thread");

    let requests = seen_requests.lock().expect("requests");
    assert_eq!(requests[0]["kind"], "hello");
    assert_eq!(requests[1]["kind"], "goodbye");
}

/// Agent unload must explicitly unregister the route from the gateway before
/// local state is cleared.
#[test]
fn gateway_client_agent_unload_sends_unregister() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let seen_requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen_requests_thread = Arc::clone(&seen_requests);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = std::io::BufReader::new(reader);
        for _ in 0..4 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            if line.trim().is_empty() {
                break;
            }
            seen_requests_thread
                .lock()
                .expect("requests")
                .push(serde_json::from_str(&line).expect("gateway request JSON"));
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "protocol_version": 3,
                    "ok": true,
                    "deliveries": [],
                })
            )
            .expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    {
        let mut state = ext.state.lock();
        state.current_session_id = Some("s1".into());
    }
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);
    let runtime = TelegramRuntime { ext };
    handle_live_event_value(
        &runtime,
        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: "s1".into(),
            agent_id: agent_id("agent-1"),
        }),
    );
    drop(runtime);
    server.join().expect("fake gateway thread");

    let requests = seen_requests.lock().expect("requests");
    assert_eq!(requests[0]["kind"], "hello");
    assert_eq!(requests[1]["kind"], "register_agent");
    assert_eq!(requests[2]["kind"], "unregister_agent");
    assert_eq!(requests[2]["session_id"], "s1");
    assert_eq!(requests[2]["agent_id"], "agent-1");
    assert_eq!(requests[3]["kind"], "goodbye");
}

/// Bot tokens are embedded in Bot API request paths, so endpoint overrides must
/// not let production plaintext or URL credentials leak the token.
#[test]
fn config_rejects_unsafe_api_base_overrides() {
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));

    for api_base in [
        "http://example.com",
        "https://user@example.com",
        "https://example.com?debug=1",
        "https://example.com/#frag",
    ] {
        let err = ExtConfig {
            bot_token_secret: Some("bot".to_owned()),
            allowed_user_ids: vec![123],
            api_base: Some(api_base.to_owned()),
            ..Default::default()
        }
        .validate(&secrets)
        .expect_err("unsafe api_base should be rejected");
        assert!(err.contains("api_base"), "{api_base}: {err}");
    }

    ExtConfig {
        bot_token_secret: Some("bot".to_owned()),
        allowed_user_ids: vec![123],
        api_base: Some("http://127.0.0.1:1234".to_owned()),
        ..Default::default()
    }
    .validate(&secrets)
    .expect("loopback http test endpoint should be allowed");
}

/// `telegram_send` is intentionally gated on prior registration so arbitrary
/// agents cannot send messages without opting into the Telegram bridge first.
#[test]
fn telegram_send_fails_before_registration() {
    let (ext, rx, _client) = extension();
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("hi")));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolError(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("telegram_register"));
}

/// A Telegram API send failure must produce a tool error without publishing a
/// preceding or later `message.sent` fact.
#[test]
fn telegram_send_transport_failure_does_not_publish_sent_fact() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    client.fail_sends("Telegram transport error");

    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("hi")));
    assert_eq!(expect_tool_error(&rx), "Telegram transport error");
    assert!(
        rx.try_recv().is_err(),
        "unexpected message.sent after failure"
    );
}

/// Registering an agent updates in-memory runtime state and lazily marks the
/// poller as started, without persisting a stale registration anywhere.
#[test]
fn telegram_register_true_registers_agent_and_starts_poller() {
    let (ext, rx, _client) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    let state = ext.state.lock();
    assert!(state.registered_agents.contains(&agent_id("agent-1")));
    assert!(state.poller_started);
}

/// Two Tau sessions using the same Telegram Bot API base and bot token would
/// race on Telegram's singleton `getUpdates` cursor, so the second registration
/// must fail closed before it starts polling and without exposing the raw
/// token.
#[test]
fn telegram_register_fails_when_update_stream_lock_is_held() {
    let root = temp_ext_root();
    let cfg = cfg();
    let (tx1, _rx1) = mpsc::channel();
    let ext1 = Extension::new(FakeClient::new(), tx1);
    ext1.apply_config(cfg.clone(), Some(root.join("std-telegram-1")))
        .expect("apply first config");
    let (tx2, rx2) = mpsc::channel();
    let ext2 = Extension::new(FakeClient::new(), tx2);
    ext2.apply_config(cfg, Some(root.join("std-telegram-2")))
        .expect("apply second config");

    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));

    let _progress = rx2.recv().expect("progress");
    let msg = rx2.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolError(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(
        error.message.contains("already locked"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("token"),
        "lock contention leaked token: {}",
        error.message
    );
    assert!(
        !ext2
            .state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-2")),
        "failed registration must not leave the agent registered"
    );
}

/// A configured Telegram webhook and getUpdates polling are mutually exclusive,
/// so registration must fail visibly instead of claiming success and leaving
/// the background poller to fail later. Tau must not delete the webhook or drop
/// pending updates on the user's behalf.
#[test]
fn telegram_register_fails_when_webhook_is_active() {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::with_webhook_info(Ok(TgWebhookInfo {
        url: "https://example.invalid/hook".to_owned(),
        pending_update_count: Some(7),
        last_error_message: Some("delivery failed".to_owned()),
    }));
    let ext = Extension::new(client, tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));

    let message = expect_tool_error(&rx);
    assert!(message.contains("active webhook"), "{message}");
    assert!(message.contains("did not delete"), "{message}");
    assert!(message.contains("7 pending"), "{message}");
    assert!(
        !ext.state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-1"))
    );
}

/// If webhook status cannot be checked, registration fails closed so the tool
/// result cannot imply that Tau owns Telegram's singleton update stream.
#[test]
fn telegram_register_fails_when_webhook_preflight_fails() {
    let (tx, rx) = mpsc::channel();
    let ext = Extension::new(
        FakeClient::with_webhook_info(Err("Telegram transport error".to_owned())),
        tx,
    );
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));

    let message = expect_tool_error(&rx);
    assert!(
        message.contains("could not verify Telegram webhook status"),
        "{message}"
    );
    assert!(
        !ext.state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-1"))
    );
}

/// Once Tau already owns and polls the update stream, additional local agents
/// should not lose ownership because a later webhook status check fails.
/// Runtime webhook/consumer contention after ownership is detected reactively
/// through `getUpdates` errors.
#[test]
fn additional_registration_does_not_drop_existing_stream_ownership_on_webhook_state() {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::with_webhook_info(Ok(TgWebhookInfo::default()));
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);
    *client.webhook_info.lock().expect("lock") = Ok(TgWebhookInfo {
        url: "https://example.invalid/hook".to_owned(),
        pending_update_count: None,
        last_error_message: None,
    });
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));
    expect_tool_finished(&rx);

    let state = ext.state.lock();
    assert!(state.registered_agents.contains(&agent_id("agent-1")));
    assert!(state.registered_agents.contains(&agent_id("agent-2")));
    assert!(state.update_stream_lock.is_some());
}

/// The advisory lock identity includes the bot token as hashed input, not just
/// the API base, so independent bots served from the same endpoint can poll
/// concurrently.
#[test]
fn update_stream_lock_allows_different_bot_tokens() {
    let root = temp_ext_root();
    let (tx1, _rx1) = mpsc::channel();
    let ext1 = Extension::new(FakeClient::new(), tx1);
    ext1.apply_config(cfg(), Some(root.join("std-telegram-1")))
        .expect("apply first config");
    let (tx2, rx2) = mpsc::channel();
    let ext2 = Extension::new(FakeClient::new(), tx2);
    let mut second_cfg = cfg();
    second_cfg.bot_token = "other-secret-token".to_owned();
    ext2.apply_config(second_cfg, Some(root.join("std-telegram-2")))
        .expect("apply second config");

    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));

    expect_tool_finished(&rx2);
    assert!(
        ext2.state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-2"))
    );
}

/// After the final local agent unregisters and the poller returns to idle, the
/// stream lock must be released so another Tau process can take over. A later
/// re-registration in the original process must then reacquire the lock and
/// fail closed if that other process still owns the stream.
#[test]
fn register_after_idle_must_reacquire_update_stream_lock() {
    let root = temp_ext_root();
    let (tx1, rx1) = mpsc::channel();
    let ext1 = Extension::new(FakeClient::new(), tx1);
    ext1.apply_config(cfg(), Some(root.join("std-telegram-1")))
        .expect("apply first config");
    let (tx2, rx2) = mpsc::channel();
    let ext2 = Extension::new(FakeClient::new(), tx2);
    ext2.apply_config(cfg(), Some(root.join("std-telegram-2")))
        .expect("apply second config");

    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx1);
    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(false)));
    expect_tool_finished(&rx1);
    std::thread::sleep(Duration::from_millis(100));

    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));
    expect_tool_finished(&rx2);
    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let message = expect_tool_error(&rx1);
    assert!(message.contains("already locked"), "{message}");
    assert!(
        !ext1
            .state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-1"))
    );
}

/// Unregistering while a long-poll request is in flight must not release the OS
/// lock until that request has returned, otherwise another Tau process could
/// issue a concurrent `getUpdates` against the singleton Telegram cursor.
#[test]
fn in_flight_poll_keeps_update_stream_lock_after_unregister() {
    let root = temp_ext_root();
    let (tx1, rx1) = mpsc::channel();
    let client1 = ControlledPollClient::new();
    let ext1 = Extension::new(client1.clone(), tx1);
    ext1.apply_config(cfg(), Some(root.join("std-telegram-1")))
        .expect("apply first config");
    let (tx2, rx2) = mpsc::channel();
    let ext2 = Extension::new(FakeClient::new(), tx2);
    ext2.apply_config(cfg(), Some(root.join("std-telegram-2")))
        .expect("apply second config");

    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx1);
    client1.wait_for_call();
    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(false)));
    expect_tool_finished(&rx1);

    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));
    let message = expect_tool_error(&rx2);
    assert!(message.contains("already locked"), "{message}");

    client1.release_first_response(Vec::new());
    std::thread::sleep(Duration::from_millis(100));
    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));
    expect_tool_finished(&rx2);
}

/// Telegram reports out-of-band long-poll contention as HTTP 409 conflicts. The
/// background poller must turn that into a user-visible diagnostic and clear
/// the active registration instead of silently leaving the agent apparently
/// connected.
#[test]
fn get_updates_409_conflict_emits_notice_and_unregisters_agents() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);
    client.wait_for_call();
    client.release_error(
        "Telegram returned HTTP 409: Conflict: terminated by other getUpdates request; \
         make sure that only one bot instance is running",
    );

    let notice = expect_notice(&rx);
    assert_eq!(notice.kind, tau_proto::notice_kind::EXTENSION_NOTICE);
    assert_eq!(notice.level, tau_proto::NoticeLevel::Warning);
    assert!(!notice.always_show);
    assert!(
        notice.message.contains("another long-poll consumer"),
        "{}",
        notice.message
    );
    assert!(
        notice.message.contains("stopped Telegram polling"),
        "{}",
        notice.message
    );
    let state = ext.state.lock();
    assert!(state.registered_agents.is_empty());
    assert!(state.update_stream_lock.is_none());
}

/// 409 conflict classification must remain robust enough to distinguish the
/// actionable webhook and competing-long-poll cases while ignoring unrelated
/// transient polling failures.
#[test]
fn telegram_contention_diagnostic_classifies_409_conflicts() {
    let cases = [
        (
            "Telegram returned HTTP 409: Conflict: terminated by setWebhook request",
            Some("webhook"),
        ),
        (
            "Telegram returned HTTP 409: Conflict: terminated by other getUpdates request; make sure that only one bot instance is running",
            Some("another long-poll consumer"),
        ),
        (
            "Telegram returned HTTP 409: Conflict: unknown",
            Some("HTTP 409 conflict"),
        ),
        ("Telegram transport error", None),
    ];

    for (input, expected) in cases {
        let diagnostic = telegram_contention_diagnostic(input);
        match expected {
            Some(expected) => assert!(
                diagnostic
                    .as_deref()
                    .is_some_and(|text| text.contains(expected)),
                "{input}: {diagnostic:?}"
            ),
            None => assert_eq!(diagnostic, None, "{input}"),
        }
    }
}

/// Webhook error text is Telegram-provided diagnostic content, so it must be
/// bounded and stripped of non-whitespace control characters before being shown
/// to the user.
#[test]
fn webhook_active_message_bounds_and_sanitizes_last_error() {
    let message = webhook_active_message(&TgWebhookInfo {
        url: "https://example.invalid/hook".to_owned(),
        pending_update_count: None,
        last_error_message: Some(format!("bad\u{1b}{}", "x".repeat(2000))),
    });

    assert!(message.contains("bad�"));
    assert!(message.ends_with('…'));
    assert!(message.len() < 1300, "message too long: {}", message.len());
}

/// Active reconfiguration to a Telegram stream already locked by another Tau
/// process must fail closed: no raw token in diagnostics, no stale
/// registration, and no old config left available for later sends.
#[test]
fn active_reconfigure_to_locked_stream_fails_closed() {
    let root = temp_ext_root();
    let (tx1, rx1) = mpsc::channel();
    let ext1 = Extension::new(FakeClient::new(), tx1);
    ext1.apply_config(cfg(), Some(root.join("std-telegram-1")))
        .expect("apply first config");
    let (tx2, rx2) = mpsc::channel();
    let ext2 = Extension::new(FakeClient::new(), tx2);
    let mut locked_cfg = cfg();
    locked_cfg.bot_token = "super-secret-telegram-token".to_owned();
    ext2.apply_config(locked_cfg.clone(), Some(root.join("std-telegram-2")))
        .expect("apply second config");

    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx1);
    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));
    expect_tool_finished(&rx2);

    let message = ext1
        .apply_config(locked_cfg, Some(root.join("std-telegram-1")))
        .expect_err("active reconfigure to locked stream should fail");
    assert!(message.contains("already locked"), "{message}");
    assert!(
        !message.contains("super-secret-telegram-token"),
        "lock contention leaked token: {message}"
    );
    {
        let state = ext1.state.lock();
        assert!(state.config.is_none());
        assert!(state.registered_agents.is_empty());
    }

    ext1.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("stale send")));
    let message = expect_tool_error(&rx1);
    assert!(message.contains("telegram_register"), "{message}");
}

/// Messages from users outside the allowlist must not become Tau prompts.
#[test]
fn incoming_unallowed_user_is_not_routed() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 999,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
}

/// Attachments without text or captions must be acknowledged as unsupported
/// instead of being silently dropped, so allowlisted Telegram users know no Tau
/// prompt was routed.
#[test]
fn textless_allowed_message_gets_unsupported_reply() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));

    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: None,
            }),
        },
    );

    assert!(rx.try_recv().is_err());
    assert_eq!(
        client.sent.lock().expect("lock")[0].1,
        "Only text messages are supported by this Tau bridge."
    );
}

/// With exactly one registered agent, plain Telegram text publishes a direct
/// delivered fact with transport-neutral source metadata.
#[test]
fn one_registered_agent_routes_plain_text() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: Some("alice".to_owned()),
                text: Some("hello".to_owned()),
            }),
        },
    );
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    let Event::MessageDelivered(req) = *emit.event else {
        panic!("message.delivered fact")
    };
    assert_eq!(req.agent_id.as_str(), "agent-1");
    assert_eq!(req.text, "hello");
    assert_eq!(req.sender.stable_id, telegram_sender_ref("123"));
    assert_eq!(
        req.sender.sender_auth,
        Some(MessageSenderAuth::VerifiedAllowlisted)
    );
    assert_eq!(
        req.conversation
            .as_ref()
            .and_then(|value| value.alias.as_ref()),
        None
    );
    assert_eq!(
        req.conversation.as_ref().expect("conversation").stable_id,
        "123"
    );
    assert_delivered_live_replay_parity(req);
}

/// Multiple registered agents without selection are ambiguous, so the bridge
/// replies with guidance instead of guessing a Tau target.
#[test]
fn multiple_agents_without_selection_do_not_route() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock")[0].1.contains("Multiple"));
}

/// Bot-facing command replies must make `agent_id` the primary designator so
/// users copy stable ids into `/select` and `/to`, with display names only as
/// parenthetical context.
#[test]
fn bot_commands_show_agent_id_before_display_name() {
    let (ext, _rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
        state
            .agent_labels
            .insert(agent_id("agent-1"), "Alpha".to_owned());
        state
            .agent_labels
            .insert(agent_id("agent-2"), "Beta".to_owned());
    }

    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("/agents".to_owned()),
            }),
        },
    );
    process_update(
        &ext,
        TgUpdate {
            update_id: 2,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("/select agent-2".to_owned()),
            }),
        },
    );

    let sent = client.sent.lock().expect("lock");
    assert_eq!(
        sent[0].1,
        "Registered Tau agents:\n- agent-1 (Alpha)\n- agent-2 (Beta)"
    );
    assert_eq!(sent[1].1, "Selected agent-2 (Beta)");
}

/// Agent ids should stand alone in `/agents` output when a display name is
/// missing, blank, or identical to the id, avoiding noisy duplicate context.
#[test]
fn agents_list_omits_empty_or_duplicate_display_names() {
    let (ext, _rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
        state.registered_agents.insert(agent_id("agent-3"));
        state
            .agent_labels
            .insert(agent_id("agent-2"), "   ".to_owned());
        state
            .agent_labels
            .insert(agent_id("agent-3"), "agent-3".to_owned());
    }

    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("/agents".to_owned()),
            }),
        },
    );

    assert_eq!(
        client.sent.lock().expect("lock")[0].1,
        "Registered Tau agents:\n- agent-1\n- agent-2\n- agent-3"
    );
}

/// Unknown or malformed slash commands must get command feedback instead of
/// being routed as ordinary prompts across the external-input boundary.
#[test]
fn malformed_slash_commands_are_not_routed_as_prompts() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    for (update_id, text) in [(1, "/startx"), (2, "/select"), (3, "/to")] {
        process_update(
            &ext,
            TgUpdate {
                update_id,
                message: Some(TgMessage {
                    chat_id: 123,
                    chat_type: None,
                    user_id: 123,
                    from_name: None,
                    text: Some(text.to_owned()),
                }),
            },
        );
    }
    assert!(rx.try_recv().is_err());
    let sent = client.sent.lock().expect("lock");
    assert!(sent[0].1.contains("Unknown"));
    assert!(sent[1].1.contains("Usage: /select"));
    assert!(sent[2].1.contains("Usage: /to"));
}

/// `/select` stores a chat-local target so later plain text can be routed even
/// while multiple agents are registered.
#[test]
fn select_then_plain_text_routes_to_selected_agent() {
    let (ext, rx, _client) = extension();
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("/select agent-2".to_owned()),
            }),
        },
    );
    process_update(
        &ext,
        TgUpdate {
            update_id: 2,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("hi".to_owned()),
            }),
        },
    );
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    let Event::MessageDelivered(req) = *emit.event else {
        panic!("message.delivered fact")
    };
    assert_eq!(req.agent_id.as_str(), "agent-2");
}

/// Runtime argument validation must match the schema so a model cannot rely on
/// ignored extra fields that may later gain meaning.
#[test]
fn telegram_send_rejects_unknown_chat_id_argument() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state
            .agent_labels
            .insert(agent_id("agent-1"), "Helper".to_owned());
    }
    let args = CborValue::Map(vec![
        (
            CborValue::Text("message".to_owned()),
            CborValue::Text("hello".to_owned()),
        ),
        (
            CborValue::Text("chat_id".to_owned()),
            CborValue::Integer(999.into()),
        ),
    ]);
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", args));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolError(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("unknown argument"));
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Group chats are refused unless the user explicitly configured that chat id;
/// this keeps the MVP private-chat oriented by default.
#[test]
fn unconfigured_group_chat_is_refused() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = None;
        state.learned_chat = None;
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: -100,
                chat_type: Some("supergroup".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(
        client.sent.lock().expect("lock")[0]
            .1
            .contains("Group chats")
    );
}

/// Explicitly configured group chat ids are allowed, while the model still does
/// not get to choose a destination for outgoing messages.
#[test]
fn configured_group_chat_can_route() {
    let (ext, rx, _client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = Some(-100);
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: -100,
                chat_type: Some("supergroup".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    assert!(matches!(*emit.event, Event::MessageDelivered(_)));
}

/// When a fixed chat is configured, allowlisted messages from any other private
/// chat must not route into Tau because replies would go to the configured
/// chat.
#[test]
fn configured_chat_rejects_other_private_chat() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 456,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(
        client.sent.lock().expect("lock")[0]
            .1
            .contains("different Telegram chat")
    );
}

/// Without a configured chat, ordinary text must wait for an explicit `/start`
/// link so the extension has a single active reply destination.
#[test]
fn unconfigured_private_text_before_start_does_not_route() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = None;
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock")[0].1.contains("/start"));
}

/// Direct `/to` routing must also wait for a linked chat; otherwise a prompt
/// submitted before `/start` could later receive replies in a different chat.
#[test]
fn unconfigured_to_before_start_does_not_route() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = None;
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("/to agent-1 hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock")[0].1.contains("/start"));
}

/// A learned private chat is exclusive; another allowlisted private chat cannot
/// redirect future `telegram_send` output or route prompts through the bridge.
#[test]
fn linked_chat_rejects_other_private_chat() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = None;
        state
            .config
            .as_mut()
            .expect("config")
            .allowed_user_ids
            .insert(456);
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("/start".to_owned()),
            }),
        },
    );
    process_update(
        &ext,
        TgUpdate {
            update_id: 2,
            message: Some(TgMessage {
                chat_id: 456,
                chat_type: Some("private".to_owned()),
                user_id: 456,
                from_name: None,
                text: Some("/start".to_owned()),
            }),
        },
    );
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    let sent_fact = expect_successful_send(&rx);

    let sent = client.sent.lock().expect("lock");
    assert_eq!(sent[0].0, 123);
    assert_eq!(sent[1].0, 456);
    assert_eq!(sent[2], (123, "[agent-1] reply".to_owned()));
    assert_eq!(sent_fact.text, "reply");
    assert_eq!(
        sent_fact.conversation.expect("conversation").stable_id,
        "123"
    );
}

/// Applying a new fixed chat invalidates registrations so replies for prompts
/// from the old active chat fail closed until agents explicitly re-register.
#[test]
fn reconfigured_chat_id_requires_reregistration_before_send() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = None;
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("/start".to_owned()),
            }),
        },
    );
    let mut new_cfg = cfg();
    new_cfg.configured_chat_id = Some(456);
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolError(error) = *emit.event else {
        panic!("tool error")
    };

    assert!(error.message.contains("telegram_register"));
    assert!(
        !client
            .sent
            .lock()
            .expect("lock")
            .iter()
            .any(|sent| sent.0 == 456)
    );
}

/// Allowlist checks run before group handling, so an unallowed group user
/// cannot trigger either a Tau prompt or a Telegram reply from the bridge.
#[test]
fn unallowed_group_user_cannot_route() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: -100,
                chat_type: Some("supergroup".to_owned()),
                user_id: 999,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// The first poll after lazy startup drains Telegram backlog without side
/// effects so old pre-registration messages do not become fresh Tau prompts.
#[test]
fn initial_poller_drops_stale_backlog() {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::with_updates(vec![vec![TgUpdate {
        update_id: 10,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("old".to_owned()),
        }),
    }]]);
    let ext = Extension::new(client, tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    std::thread::sleep(Duration::from_millis(100));
    assert!(rx.try_recv().is_err());
}

/// Initial backlog draining must continue until Telegram returns an empty
/// batch; otherwise older messages split across batches could leak as fresh
/// prompts.
#[test]
fn initial_poller_drops_multiple_stale_batches_until_empty() {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::with_updates(vec![
        vec![TgUpdate {
            update_id: 10,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("old one".to_owned()),
            }),
        }],
        vec![TgUpdate {
            update_id: 11,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("old two".to_owned()),
            }),
        }],
        Vec::new(),
        vec![TgUpdate {
            update_id: 12,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: Some("alice".to_owned()),
                text: Some("fresh".to_owned()),
            }),
        }],
    ]);
    let ext = Extension::new(client, tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("fresh prompt") else {
        panic!("emit")
    };
    let Event::MessageDelivered(req) = *emit.event else {
        panic!("message.delivered fact")
    };
    assert_eq!(req.text, "fresh");
}

/// Telegram updates without a usable message still carry update ids and must be
/// represented so the poller can advance past them to later valid messages.
#[test]
fn decode_update_preserves_non_message_update_id() {
    let update = decode_update(&serde_json::json!({ "update_id": 42 }))
        .expect("update id should be preserved");
    assert_eq!(update.update_id, 42);
    assert_eq!(update.message, None);
}

/// HTTP transport errors must not include Telegram Bot API URLs because those
/// URLs contain the bot token in their path.
#[test]
fn telegram_transport_errors_do_not_expose_bot_token() {
    let client = HttpTelegramClient::default();
    let mut cfg = cfg();
    cfg.bot_token = "secret-token-for-test".to_owned();
    cfg.api_base = "http://127.0.0.1:9".to_owned();
    let err = client
        .send_message(&cfg, 123, "hello")
        .expect_err("connection should fail");
    assert!(!err.contains("secret-token-for-test"), "err: {err}");
}

/// Registering starts a poller, and disconnect/EOF-facing shutdown must not
/// hang waiting for leaked sender clones held by that poller.
#[test]
fn run_exits_after_register_then_disconnect() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
        }))
        .expect("config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("tool");
    writer
        .write_message(&HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: None,
        }))
        .expect("disconnect");
    writer.flush().expect("flush");

    run_with_client(std::io::Cursor::new(input), Vec::new(), FakeClient::new()).expect("run");
}

/// Disconnect handling must not wait for an in-flight long poll to release its
/// channel sender before the extension process can exit.
#[test]
fn run_exits_promptly_when_disconnect_races_long_poll() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
        }))
        .expect("config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("tool");
    writer
        .write_message(&HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: None,
        }))
        .expect("disconnect");
    writer.flush().expect("flush");

    let start = std::time::Instant::now();
    run_with_client(
        std::io::Cursor::new(input),
        Vec::new(),
        Arc::new(SlowPollClient),
    )
    .expect("run");
    assert!(start.elapsed() < Duration::from_secs(1));
}

/// Replayed tool deliveries must be skipped so historical registrations do not
/// restart the Telegram bridge or authorize later live sends.
#[test]
fn run_ignores_replayed_tool_delivery_before_live_send() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
        }))
        .expect("config");
    writer
        .write_message(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1_700_000_000_000_000),
            Event::ToolStarted(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true))),
        ))
        .expect("replay register");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            SEND_TOOL_NAME,
            "agent-1",
            message_args("reply"),
        ))))
        .expect("live send");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    let client = FakeClient::new();
    run_with_client(std::io::Cursor::new(input), output, client.clone()).expect("run");

    let mut reader = HarnessInputReader::new(std::io::Cursor::new(written.bytes()));
    let mut saw_unregistered_error = false;
    while let Some(frame) = reader.read_message().expect("read output") {
        if let HarnessInputMessage::Emit(emit) = frame
            && let Event::ToolError(error) = emit.event.as_ref()
            && error.tool_name.as_str() == SEND_TOOL_NAME
            && error.message.contains("telegram_register")
        {
            saw_unregistered_error = true;
        }
    }
    assert!(
        saw_unregistered_error,
        "live send should fail without live registration"
    );
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Initial malformed configuration must still surface as ConfigError and Ready
/// in deferred-startup mode, rather than becoming a silent extension startup
/// failure before the harness can publish a replayable notice.
#[test]
fn run_initial_malformed_config_emits_config_error_without_ready() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "unknown_field": true,
            })),
            state_dir: Some(temp_state_dir()),
            secrets: BTreeMap::new(),
        }))
        .expect("invalid config");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    run_with_client(std::io::Cursor::new(input), output, FakeClient::new()).expect("run");

    let mut reader = HarnessInputReader::new(std::io::Cursor::new(written.bytes()));
    let mut saw_config_error = false;
    let mut saw_ready = false;
    while let Some(frame) = reader.read_message().expect("read output") {
        match frame {
            HarnessInputMessage::ConfigError(error) if error.message.contains("unknown_field") => {
                saw_config_error = true;
            }
            HarnessInputMessage::Ready(_) => saw_ready = true,
            _ => {}
        }
    }
    assert!(saw_config_error, "initial config error should be reported");
    assert!(!saw_ready, "rejected initial config must withhold Ready");
}

/// The protocol startup path must publish and dispatch the dynamically computed
/// namespaced tools, not only construct the helper structs used by unit tests.
#[test]
fn run_custom_instance_registers_and_dispatches_namespaced_tools() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: Some(tau_proto::ToolNamePrefix::parse("work").expect("prefix")),
            instance_name: tau_proto::ExtensionName::new("telegram-work"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
        }))
        .expect("config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            "work_telegram_register",
            "agent-1",
            bool_args(true),
        ))))
        .expect("register");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    run_with_client(std::io::Cursor::new(input), output, FakeClient::new()).expect("run");

    let mut reader = HarnessInputReader::new(std::io::Cursor::new(written.bytes()));
    let mut saw_register_tool = false;
    let mut saw_send_tool = false;
    let mut saw_register_result = false;
    while let Some(frame) = reader.read_message().expect("read output") {
        if let HarnessInputMessage::Emit(emit) = frame {
            match emit.event.as_ref() {
                Event::ToolRegister(register)
                    if register.tool.name.as_str() == "work_telegram_register"
                        && register
                            .tool_group
                            .as_ref()
                            .is_some_and(|group| group.name.as_str() == "work_telegram") =>
                {
                    saw_register_tool = true;
                }
                Event::ToolRegister(register)
                    if register.tool.name.as_str() == "work_telegram_send" =>
                {
                    saw_send_tool = true;
                }
                Event::ToolResult(result)
                    if result.tool_name.as_str() == "work_telegram_register" =>
                {
                    saw_register_result = true;
                }
                _ => {}
            }
        }
    }
    assert!(
        saw_register_tool,
        "namespaced register tool should be published"
    );
    assert!(saw_send_tool, "namespaced send tool should be published");
    assert!(
        saw_register_result,
        "namespaced register invocation should dispatch"
    );
}

/// Manual deferred dispatch must preserve tau-client's previous named-handler
/// filtering: unrelated tool calls, including tools owned by another Telegram
/// instance, are not Telegram calls and must not receive Telegram progress or
/// errors.
#[test]
fn run_ignores_unrelated_tool_started_events() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
        }))
        .expect("config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            "other_tool",
            "agent-1",
            CborValue::Map(Vec::new()),
        ))))
        .expect("other tool");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    run_with_client(std::io::Cursor::new(input), output, FakeClient::new()).expect("run");

    let mut reader = HarnessInputReader::new(std::io::Cursor::new(written.bytes()));
    while let Some(frame) = reader.read_message().expect("read output") {
        if let HarnessInputMessage::Emit(emit) = frame {
            match emit.event.as_ref() {
                Event::ToolProgress(progress) if progress.tool_name.as_str() == "other_tool" => {
                    panic!("unrelated tool should not receive Telegram progress");
                }
                Event::ToolError(error) if error.tool_name.as_str() == "other_tool" => {
                    panic!("unrelated tool should not receive Telegram error");
                }
                Event::ToolResult(result) if result.tool_name.as_str() == "other_tool" => {
                    panic!("unrelated tool should not receive Telegram result");
                }
                _ => {}
            }
        }
    }
}

/// Malformed reconfiguration that fails typed deserialization must fail closed:
/// emit `ConfigError`, clear registrations/config, and prevent later sends from
/// using stale Telegram routing state.
#[test]
fn run_malformed_reconfiguration_clears_active_bridge_state() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
        }))
        .expect("valid config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("live register");
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "unknown_field": true,
            })),
            state_dir: Some(temp_state_dir()),
            secrets: BTreeMap::new(),
        }))
        .expect("invalid config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            SEND_TOOL_NAME,
            "agent-1",
            message_args("reply"),
        ))))
        .expect("live send");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    let client = FakeClient::new();
    run_with_client(std::io::Cursor::new(input), output, client.clone()).expect("run");

    let mut reader = HarnessInputReader::new(std::io::Cursor::new(written.bytes()));
    let mut saw_config_error = false;
    let mut saw_unregistered_error = false;
    while let Some(frame) = reader.read_message().expect("read output") {
        match frame {
            HarnessInputMessage::ConfigError(error) if error.message.contains("unknown_field") => {
                saw_config_error = true;
            }
            HarnessInputMessage::Emit(emit) => {
                if let Event::ToolError(error) = emit.event.as_ref()
                    && error.tool_name.as_str() == SEND_TOOL_NAME
                    && error.message.contains("telegram_register")
                {
                    saw_unregistered_error = true;
                }
            }
            _ => {}
        }
    }
    assert!(saw_config_error, "malformed config should emit ConfigError");
    assert!(
        saw_unregistered_error,
        "send should fail after malformed config clears registration"
    );
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Removed legacy `tool_namespace` configuration is rejected rather than
/// silently restoring the superseded Telegram-specific naming mechanism.
#[test]
fn run_legacy_tool_namespace_is_rejected() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
            })),
            state_dir: Some(temp_state_dir()),
            secrets: secrets.clone(),
        }))
        .expect("valid config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("register");
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "tool_namespace": "tg_ops",
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
        }))
        .expect("namespace config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            SEND_TOOL_NAME,
            "agent-1",
            message_args("reply"),
        ))))
        .expect("send");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    let client = FakeClient::new();
    run_with_client(std::io::Cursor::new(input), output, client.clone()).expect("run");

    let mut reader = HarnessInputReader::new(std::io::Cursor::new(written.bytes()));
    let mut saw_config_error = false;
    let mut saw_send_error = false;
    while let Some(frame) = reader.read_message().expect("read output") {
        match frame {
            HarnessInputMessage::ConfigError(error) if error.message.contains("tool_namespace") => {
                saw_config_error = true;
            }
            HarnessInputMessage::Emit(emit) => {
                if let Event::ToolError(error) = emit.event.as_ref()
                    && error.tool_name.as_str() == SEND_TOOL_NAME
                    && error.message.contains("telegram_register")
                {
                    saw_send_error = true;
                }
            }
            _ => {}
        }
    }
    assert!(
        saw_config_error,
        "legacy tool_namespace should emit ConfigError"
    );
    assert!(
        saw_send_error,
        "send should fail after namespace config error"
    );
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Initial backlog drain must be a non-long-poll request. Otherwise a fresh
/// message arriving during the first long poll after registration could be
/// mistaken for stale backlog and dropped.
#[test]
fn initial_empty_drain_then_fresh_message_routes() {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::with_updates(vec![
        Vec::new(),
        vec![TgUpdate {
            update_id: 11,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: Some("alice".to_owned()),
                text: Some("fresh".to_owned()),
            }),
        }],
    ]);
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("fresh prompt") else {
        panic!("emit")
    };
    let Event::MessageDelivered(req) = *emit.event else {
        panic!("message.delivered fact")
    };
    assert_eq!(req.text, "fresh");
    assert_eq!(client.poll_timeouts.lock().expect("lock")[0], 0);
}

/// Switching to a different Telegram bot token changes the update stream, so
/// the extension must reset its offset and drain that bot's existing backlog
/// before routing fresh messages.
#[test]
fn reconfigured_bot_token_resets_update_backlog_drain() {
    let (ext, _rx, _client) = extension();
    {
        let mut state = ext.state.lock();
        state.poller_drained_initial_backlog = true;
        state.next_update_offset = Some(99);
    }

    let mut new_cfg = cfg();
    new_cfg.bot_token = "different-token".to_owned();
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");

    let state = ext.state.lock();
    assert!(!state.poller_drained_initial_backlog);
    assert_eq!(state.next_update_offset, None);
}

/// Changing the Bot API endpoint also changes the update stream, so stale
/// offsets from the previous endpoint must be dropped.
#[test]
fn reconfigured_api_base_resets_update_backlog_drain() {
    let (ext, _rx, _client) = extension();
    {
        let mut state = ext.state.lock();
        state.poller_drained_initial_backlog = true;
        state.next_update_offset = Some(99);
    }

    let mut new_cfg = cfg();
    new_cfg.api_base = "http://127.0.0.1:1234".to_owned();
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");

    let state = ext.state.lock();
    assert!(!state.poller_drained_initial_backlog);
    assert_eq!(state.next_update_offset, None);
}

/// Tuning poll timeout alone does not change the Telegram update stream, so the
/// extension should keep the acknowledged offset and avoid redraining already
/// processed updates.
#[test]
fn reconfigured_poll_timeout_keeps_update_offset() {
    let (ext, _rx, _client) = extension();
    {
        let mut state = ext.state.lock();
        state.poller_drained_initial_backlog = true;
        state.next_update_offset = Some(99);
    }

    let mut new_cfg = cfg();
    new_cfg.poll_timeout_seconds = 5;
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");

    let state = ext.state.lock();
    assert!(state.poller_drained_initial_backlog);
    assert_eq!(state.next_update_offset, Some(99));
}

/// Poll responses captured under an older config generation must be discarded
/// after reconfiguration so old-stream updates cannot advance or drain the new
/// stream.
#[test]
fn old_generation_empty_poll_response_does_not_drain_new_stream() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    client.wait_for_call();

    let mut new_cfg = cfg();
    new_cfg.bot_token = "different-token".to_owned();
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");
    client.release_first_response(Vec::new());
    std::thread::sleep(Duration::from_millis(100));

    let state = ext.state.lock();
    assert!(!state.poller_drained_initial_backlog);
    assert_eq!(state.next_update_offset, None);
    assert!(rx.try_recv().is_err());
}

/// Non-empty poll responses from an old config generation must also be
/// discarded, avoiding both stale offset updates and fact publication under
/// the new config.
#[test]
fn old_generation_non_empty_poll_response_does_not_route_or_advance_offset() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    client.wait_for_call();

    let mut new_cfg = cfg();
    new_cfg.api_base = "http://127.0.0.1:1234".to_owned();
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");
    client.release_first_response(vec![TgUpdate {
        update_id: 55,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: Some("alice".to_owned()),
            text: Some("stale".to_owned()),
        }),
    }]);
    std::thread::sleep(Duration::from_millis(100));

    let state = ext.state.lock();
    assert!(!state.poller_drained_initial_backlog);
    assert_eq!(state.next_update_offset, None);
    assert!(rx.try_recv().is_err());
}

/// A period with no registered agents is a stale-backlog boundary: Telegram
/// messages observed while nobody is listening must advance offsets but must
/// not route after a later registration.
#[test]
fn zero_registered_agents_redrains_backlog_before_routing() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);

    client.wait_for_call_count(1);
    client.release_first_response(Vec::new());
    client.wait_for_call_count(2);

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(false)));
    expect_tool_finished(&rx);
    client.release_first_response(vec![TgUpdate {
        update_id: 20,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: Some("alice".to_owned()),
            text: Some("stale while unregistered".to_owned()),
        }),
    }]);
    assert!(rx.try_recv().is_err());

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);
    client.wait_for_call_count(3);
    client.release_first_response(Vec::new());
    client.wait_for_call_count(4);
    client.release_first_response(vec![TgUpdate {
        update_id: 21,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: Some("alice".to_owned()),
            text: Some("fresh after reregister".to_owned()),
        }),
    }]);

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("fresh prompt") else {
        panic!("emit")
    };
    let Event::MessageDelivered(req) = *emit.event else {
        panic!("message.delivered fact")
    };
    assert_eq!(req.text, "fresh after reregister");
}

/// Error backoff uses the shared-state condvar so a config change wakes it
/// promptly instead of waiting for the full local retry delay.
#[test]
fn poll_error_backoff_wakes_on_config_change() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);

    client.wait_for_call_count(1);
    client.release_first_response(Vec::new());
    client.wait_for_call_count(2);
    client.release_error("temporary failure");

    let mut new_cfg = cfg();
    new_cfg.poll_timeout_seconds = 2;
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");
    client.wait_for_call_count(3);
}

/// Shutdown is recorded under the same mutex used by the poller readiness
/// condvar, so a poller parked with no registered agents cannot miss the
/// wakeup.
#[test]
fn shutdown_wakes_poller_readiness_wait() {
    let (tx, _rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = Extension::new(client.clone(), tx.clone());
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    let state = Arc::clone(&ext.state);
    let shutdown = Arc::clone(&ext.shutdown);
    let handle = std::thread::spawn(move || poll_loop(state, client, tx.into(), shutdown));

    std::thread::sleep(Duration::from_millis(50));
    let start = std::time::Instant::now();
    ext.request_shutdown();
    handle.join().expect("poller joins after shutdown");
    assert!(start.elapsed() < Duration::from_secs(1));
}

/// Even after the poll loop's first generation check succeeds, a later config
/// change before per-update processing must stop stale updates from routing
/// through the new current config.
#[test]
fn stale_generation_update_processing_does_not_reread_current_config() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    let old_generation = ext.state.lock().config_generation;
    assert!(ext.poll_response_matches_config(old_generation));

    let mut new_cfg = cfg();
    new_cfg.bot_token = "different-token".to_owned();
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));

    ext.process_update_for_generation(
        TgUpdate {
            update_id: 55,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: Some("alice".to_owned()),
                text: Some("stale".to_owned()),
            }),
        },
        old_generation,
    );

    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Invalid reconfiguration fails closed: previous registrations and chat state
/// are cleared so neither old Telegram messages nor agent sends keep using the
/// previous access policy.
#[test]
fn invalid_reconfiguration_clears_active_bridge_state() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));

    ext.clear_config_after_error();
    process_update(
        &ext,
        TgUpdate {
            update_id: 1,
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());

    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolError(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("telegram_register"));
}
