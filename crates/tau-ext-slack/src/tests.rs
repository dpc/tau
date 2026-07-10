use std::io::Write;
use std::sync::Mutex;

use tau_proto::{HarnessInputMessage, HarnessOutputMessage, ToolStarted};

use super::*;

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

/// One complete fake Slack post recorded atomically for assertions.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SentMessage {
    /// Destination conversation.
    channel_id: String,
    /// Posted text.
    text: String,
    /// Optional originating thread root.
    thread_ts: Option<String>,
}

struct FakeClient {
    sent: Mutex<Vec<SentMessage>>,
    open_count: Mutex<usize>,
    auth_count: Mutex<usize>,
}

impl FakeClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            open_count: Mutex::new(0),
            auth_count: Mutex::new(0),
        })
    }

    /// Return channel/text pairs for assertions that predate thread routing.
    fn sent_pairs(&self) -> Vec<(String, String)> {
        self.sent
            .lock()
            .expect("lock")
            .iter()
            .map(|message| (message.channel_id.clone(), message.text.clone()))
            .collect()
    }

    /// Return thread destinations from the same atomic post records.
    fn sent_thread_ids(&self) -> Vec<Option<String>> {
        self.sent
            .lock()
            .expect("lock")
            .iter()
            .map(|message| message.thread_ts.clone())
            .collect()
    }
}

impl SlackClient for FakeClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        *self.open_count.lock().expect("lock") += 1;
        Ok("ws://127.0.0.1:9/socket-ticket".to_owned())
    }

    fn auth_test(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        *self.auth_count.lock().expect("lock") += 1;
        Ok("UBOT123".to_owned())
    }

    fn is_human_user(&self, _cfg: &RuntimeConfig, user_id: &str) -> Result<bool, String> {
        Ok(user_id != "UBOT999")
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        channel_id: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<PostedMessage, String> {
        let mut sent = self.sent.lock().expect("lock");
        sent.push(SentMessage {
            channel_id: channel_id.to_owned(),
            text: text.to_owned(),
            thread_ts: thread_ts.map(str::to_owned),
        });
        Ok(PostedMessage {
            ts: format!("{}.0", sent.len()),
            thread_ts: None,
        })
    }
}

struct FailingAuthClient;

impl SlackClient for FailingAuthClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        Ok("ws://127.0.0.1:9/socket-ticket".to_owned())
    }

    fn auth_test(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        Err("Slack API auth.test failed: invalid_auth".to_owned())
    }

    fn is_human_user(&self, _cfg: &RuntimeConfig, _user_id: &str) -> Result<bool, String> {
        Ok(true)
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        _channel_id: &str,
        _text: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PostedMessage, String> {
        Ok(PostedMessage {
            ts: "1.0".to_owned(),
            thread_ts: None,
        })
    }
}

fn cfg() -> RuntimeConfig {
    RuntimeConfig {
        app_token: "xapp-test".to_owned(),
        bot_token: "xoxb-test".to_owned(),
        allowed_user_ids: ["U123".to_owned()].into_iter().collect(),
        configured_channel_ids: ["C123".to_owned()].into_iter().collect(),
        api_base: DEFAULT_API_BASE.to_owned(),
        max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
    }
}

fn dm_cfg() -> RuntimeConfig {
    RuntimeConfig {
        configured_channel_ids: HashSet::new(),
        ..cfg()
    }
}

fn multi_channel_cfg() -> RuntimeConfig {
    RuntimeConfig {
        configured_channel_ids: ["C123".to_owned(), "C456".to_owned()].into_iter().collect(),
        ..cfg()
    }
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

fn valid_config_message() -> HarnessOutputMessage {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    HarnessOutputMessage::Configure(tau_proto::Configure {
        instance_name: None,
        config: tau_proto::json_to_cbor(&serde_json::json!({
            "app_token_secret": "app",
            "bot_token_secret": "bot",
            "allowed_user_ids": ["U123"],
            "channel_ids": ["C123"],
            "api_base": "http://127.0.0.1:8080/api",
            "max_message_bytes": 16384,
        })),
        state_dir: None,
        secrets,
    })
}

fn malformed_config_message() -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(tau_proto::Configure {
        instance_name: None,
        config: tau_proto::json_to_cbor(&serde_json::json!({
            "unknown_field": true,
        })),
        state_dir: None,
        secrets: BTreeMap::new(),
    })
}

fn run_protocol_messages(
    messages: &[HarnessOutputMessage],
    client: Arc<FakeClient>,
) -> Vec<HarnessInputMessage> {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    for message in messages {
        writer.write_message(message).expect("write input");
    }
    writer.flush().expect("flush input");

    let output = SharedWriter::default();
    let written = output.clone();
    run_with_client(std::io::Cursor::new(input), output, client).expect("run");

    let mut frames = Vec::new();
    let mut reader = tau_proto::HarnessInputReader::new(std::io::Cursor::new(written.bytes()));
    while let Some(frame) = reader.read_message().expect("read output") {
        frames.push(frame);
    }
    frames
}

fn extension() -> (
    Extension,
    mpsc::Receiver<HarnessInputMessage>,
    Arc<FakeClient>,
) {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg()).expect("config");
    {
        let mut state = ext.state.lock().expect("lock");
        state.bot_user_id = Some("UBOT123".to_owned());
    }
    (ext, rx, client)
}

fn slack_message(channel_id: &str, channel_type: Option<&str>, text: &str) -> SlackMessage {
    SlackMessage {
        event_id: Some(format!("EV-{channel_id}-{text}")),
        channel_id: channel_id.to_owned(),
        channel_type: channel_type.map(str::to_owned),
        user_id: "U123".to_owned(),
        text: text.to_owned(),
        event_type: if channel_type == Some("im") {
            "message"
        } else {
            "app_mention"
        }
        .to_owned(),
        subtype: None,
        bot_id: None,
        ts: Some("1.0".to_owned()),
        thread_ts: None,
    }
}

fn slack_conversation(channel_id: &str, thread_ts: Option<&str>) -> SlackConversation {
    SlackConversation {
        channel_id: channel_id.to_owned(),
        thread_ts: thread_ts.map(str::to_owned),
    }
}

fn slack_reaction(
    event_id: &str,
    event_type: &str,
    channel_id: &str,
    message_ts: &str,
) -> SlackReaction {
    SlackReaction {
        event_id: Some(event_id.to_owned()),
        event_type: if event_type == "reaction_added" {
            ReactionKind::Added
        } else {
            ReactionKind::Removed
        },
        user_id: "U123".to_owned(),
        reaction: "thumbsup".to_owned(),
        channel_id: channel_id.to_owned(),
        message_ts: message_ts.to_owned(),
        thread_ts: None,
    }
}

fn register_agent(ext: &Extension, agent: &str) {
    {
        let mut state = ext.state.lock().expect("lock");
        state.registered_agents.insert(agent_id(agent));
        state.agent_labels.insert(agent_id(agent), agent.to_owned());
    }
}

fn recv_prompt(rx: &mpsc::Receiver<HarnessInputMessage>) -> String {
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("message") else {
        panic!("expected emit");
    };
    let Event::ExtPromptSubmitRequest(prompt) = *emit.event else {
        panic!("expected prompt request");
    };
    prompt.text
}

fn recv_prompt_request(
    rx: &mpsc::Receiver<HarnessInputMessage>,
) -> tau_proto::ExtPromptSubmitRequest {
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("message") else {
        panic!("expected emit");
    };
    let Event::ExtPromptSubmitRequest(prompt) = *emit.event else {
        panic!("expected prompt request");
    };
    prompt
}

fn activate_prompt_origin(ext: &Extension, prompt: &tau_proto::ExtPromptSubmitRequest) {
    ext.handle_prompt_submitted(&tau_proto::AgentPromptSubmitted {
        agent_id: prompt.agent_id.clone(),
        text: prompt.text.clone(),
        message_class: prompt.message_class,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: prompt.ctx_id.clone(),
    });
    ext.handle_prompt_started(&prompt.agent_id, prompt.ctx_id.as_deref());
}

fn steer_prompt_origin(ext: &Extension, prompt: &tau_proto::ExtPromptSubmitRequest) {
    ext.handle_prompt_steered(&tau_proto::AgentPromptSteered {
        agent_id: prompt.agent_id.clone(),
        text: prompt.text.clone(),
        message_class: prompt.message_class,
        ctx_id: prompt.ctx_id.clone(),
    });
}

/// Ensures the shared shutdown signal wakes async waiters immediately,
/// preventing regressions to periodic shutdown polling in the Slack worker.
#[tokio::test]
async fn shutdown_signal_wait_wakes_after_request() {
    let shutdown = Arc::new(ShutdownSignal::new());
    let waiter_shutdown = Arc::clone(&shutdown);
    let waiter = tokio::spawn(async move {
        waiter_shutdown.wait().await;
    });

    tokio::task::yield_now().await;
    shutdown.request();

    tokio::time::timeout(Duration::from_millis(75), waiter)
        .await
        .expect("shutdown waiter should wake promptly")
        .expect("shutdown waiter should not panic");
}

/// Ensures reconnect backoff waits are interruptible by notification rather
/// than sleeping in fixed polling chunks for the full delay.
#[tokio::test]
async fn shutdown_signal_wait_timeout_wakes_before_long_backoff() {
    let shutdown = Arc::new(ShutdownSignal::new());
    let waiter_shutdown = Arc::clone(&shutdown);
    let waiter =
        tokio::spawn(async move { waiter_shutdown.wait_timeout(Duration::from_secs(60)).await });

    tokio::task::yield_now().await;
    shutdown.request();

    let interrupted = tokio::time::timeout(Duration::from_millis(75), waiter)
        .await
        .expect("backoff wait should wake promptly")
        .expect("backoff waiter should not panic");
    assert!(interrupted, "wait_timeout should report requested shutdown");
}

/// Ensures a Socket Mode worker blocked on websocket receive exits promptly
/// when shutdown is requested, preserving shutdown latency without a receive
/// timeout.
#[tokio::test]
async fn socket_worker_once_shutdown_interrupts_idle_websocket_receive() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback websocket listener");
    let socket_url = format!(
        "ws://{}/socket-ticket",
        listener.local_addr().expect("listener local address")
    );
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept websocket client");
        let _ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete websocket handshake");
        let _ = accepted_tx.send(());
        std::future::pending::<()>().await;
    });

    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    let shutdown = Arc::clone(&ext.shutdown);
    let worker_cfg = cfg();
    let worker = tokio::spawn(async move {
        socket_worker_once(
            &ext,
            &worker_cfg,
            Some(WorkerStartup {
                bot_user_id: "UBOT123".to_owned(),
                socket_url,
            }),
        )
        .await
    });

    accepted_rx.await.expect("websocket should connect");
    shutdown.request();

    let outcome = tokio::time::timeout(Duration::from_millis(150), worker)
        .await
        .expect("socket worker should stop promptly")
        .expect("socket worker should not panic")
        .expect("socket worker should exit cleanly");
    assert_eq!(outcome, WorkerOutcome::Shutdown);
    server.abort();
}

/// Long successful Slack Web API JSON responses must be parsed from the raw
/// body rather than from bounded diagnostic text, otherwise successful sends
/// can be reported as false JSON errors.
#[test]
fn long_successful_slack_api_response_still_parses() {
    let cfg = cfg();
    let long_text = "x".repeat(MAX_DIAGNOSTIC_BYTES + 200);
    let body = serde_json::json!({
        "ok": true,
        "url": "wss://wss-primary.slack.com/link",
        "message": { "text": long_text }
    })
    .to_string();
    let value = parse_slack_api_response(&cfg, "chat.postMessage", 200, None, &body)
        .expect("long ok response parses");
    assert_eq!(
        value.get("ok").and_then(|value| value.as_bool()),
        Some(true)
    );
}

/// Slack API diagnostic responses are token-redacted and bounded without
/// affecting parsing of successful response bodies.
#[test]
fn slack_api_error_response_is_redacted_and_bounded() {
    let cfg = cfg();
    let body = serde_json::json!({
        "ok": false,
        "error": format!(
            "{} {} {}",
            cfg.app_token,
            cfg.bot_token,
            "x".repeat(MAX_DIAGNOSTIC_BYTES * 2)
        )
    })
    .to_string();
    let err =
        parse_slack_api_response(&cfg, "auth.test", 200, None, &body).expect_err("slack error");
    assert!(!err.contains(&cfg.app_token));
    assert!(!err.contains(&cfg.bot_token));
    assert!(err.len() <= MAX_DIAGNOSTIC_BYTES + 64, "{err}");
}

/// Slack bridge tools are disabled by default because roles must explicitly opt
/// into an external chat bridge before the model can use it.
#[test]
fn slack_tools_are_role_opt_in() {
    assert!(!register_tool_spec().enabled_by_default);
    assert!(!send_tool_spec().enabled_by_default);
}

/// Tool group and tag metadata let role policy enable all Slack tools or only
/// registration/sending capability.
#[test]
fn slack_tools_have_group_and_tags() {
    assert_eq!(slack_tool_group().name.as_str(), TOOL_GROUP_NAME);
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

/// Provider-owned repair examples must remain schema-valid as Slack tool
/// argument shapes evolve.
#[test]
fn slack_tool_examples_are_schema_valid() {
    for spec in [register_tool_spec(), send_tool_spec()] {
        tau_core::validate_tool_examples(&spec)
            .unwrap_or_else(|error| panic!("invalid examples for {}: {error}", spec.name));
    }
}

/// The send tool schema and runtime validation must not allow model-selected
/// Slack destinations such as channel ids.
#[test]
fn slack_send_rejects_destination_arguments() {
    let (ext, _rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let event = ext.handle_send(tool(
        SEND_TOOL_NAME,
        "agent-a",
        CborValue::Map(vec![
            (
                CborValue::Text("message".to_owned()),
                CborValue::Text("hi".to_owned()),
            ),
            (
                CborValue::Text("channel_id".to_owned()),
                CborValue::Text("C999".to_owned()),
            ),
        ]),
    ));
    let Event::ToolError(err) = event else {
        panic!("expected error");
    };
    assert!(err.message.contains("unknown argument `channel_id`"));
}

/// Config validation requires both token secret names, non-empty resolved
/// secret values, and a non-empty user allowlist before Slack can be contacted.
#[test]
fn config_rejects_missing_tokens_or_empty_allowlist() {
    let err = ExtConfig::default()
        .validate(&BTreeMap::new())
        .err()
        .expect("missing app token");
    assert!(err.contains("app_token_secret"));

    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    let err = ExtConfig {
        app_token_secret: Some("app".to_owned()),
        bot_token_secret: Some("bot".to_owned()),
        allowed_user_ids: vec!["U123".to_owned()],
        ..Default::default()
    }
    .validate(&secrets)
    .err()
    .expect("missing bot token");
    assert!(err.contains("bot token secret`bot`") || err.contains("bot token secret `bot`"));

    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new(""));
    let err = ExtConfig {
        app_token_secret: Some("app".to_owned()),
        bot_token_secret: Some("bot".to_owned()),
        allowed_user_ids: vec!["U123".to_owned()],
        ..Default::default()
    }
    .validate(&secrets)
    .err()
    .expect("empty bot token");
    assert!(err.contains("missing or empty"));

    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    let err = ExtConfig {
        app_token_secret: Some("app".to_owned()),
        bot_token_secret: Some("bot".to_owned()),
        ..Default::default()
    }
    .validate(&secrets)
    .err()
    .expect("empty allowlist");
    assert!(err.contains("allowed_user_ids"));
}

/// Unknown config keys are rejected instead of being silently ignored, because
/// a typo in chat-bridge policy should surface as a harness ConfigError.
#[test]
fn config_rejects_unknown_fields() {
    let value = tau_proto::json_to_cbor(&serde_json::json!({
        "app_token_secret": "app",
        "bot_token_secret": "bot",
        "allowed_user_ids": ["U123"],
        "destination": "C123"
    }));
    let err = value
        .deserialized::<ExtConfig>()
        .map_err(|error| format!("{error:?}"))
        .expect_err("unknown field");
    assert!(err.contains("unknown field"));
    assert!(err.contains("destination"));
}

/// Duplicate user or channel ids are most likely policy mistakes and must
/// become visible configuration errors rather than silently collapsing.
#[test]
fn config_rejects_duplicate_allowlist_entries() {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));

    for config in [
        ExtConfig {
            app_token_secret: Some("app".to_owned()),
            bot_token_secret: Some("bot".to_owned()),
            allowed_user_ids: vec!["U123".to_owned(), "U123".to_owned()],
            ..Default::default()
        },
        ExtConfig {
            app_token_secret: Some("app".to_owned()),
            bot_token_secret: Some("bot".to_owned()),
            allowed_user_ids: vec!["U123".to_owned()],
            channel_ids: vec!["C123".to_owned(), "C123".to_owned()],
            ..Default::default()
        },
    ] {
        let error = config
            .validate(&secrets)
            .err()
            .expect("duplicate id must fail");
        assert!(error.contains("duplicate id"), "{error}");
    }
}

/// Empty and malformed ids in either security allowlist fail validation rather
/// than being trimmed away or accepted as unusable policy entries.
#[test]
fn config_rejects_empty_and_malformed_allowlist_ids() {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    for (users, channels) in [
        (vec!["".to_owned()], vec![]),
        (vec!["user-lower".to_owned()], vec![]),
        (vec!["U123".to_owned()], vec!["".to_owned()]),
        (vec!["U123".to_owned()], vec!["channel-lower".to_owned()]),
    ] {
        let error = ExtConfig {
            app_token_secret: Some("app".to_owned()),
            bot_token_secret: Some("bot".to_owned()),
            allowed_user_ids: users,
            channel_ids: channels,
            ..Default::default()
        }
        .validate(&secrets)
        .err()
        .expect("invalid id must fail");
        assert!(error.contains("invalid Slack id") || error.contains("empty ids"));
    }
}

/// The obsolete singular destination key is rejected so operators cannot
/// believe a channel is authorized when the extension ignored it.
#[test]
fn config_rejects_singular_channel_id() {
    let value = tau_proto::json_to_cbor(&serde_json::json!({
        "app_token_secret": "app",
        "bot_token_secret": "bot",
        "allowed_user_ids": ["U123"],
        "channel_id": "C123"
    }));
    let error = value
        .deserialized::<ExtConfig>()
        .map_err(|error| format!("{error:?}"))
        .expect_err("obsolete singular key");
    assert!(error.contains("unknown field"));
    assert!(error.contains("channel_id"));
}

/// Slack Web API endpoint overrides must not downgrade production traffic or
/// smuggle credentials/query data into diagnostics; loopback HTTP remains
/// usable for tests.
#[test]
fn config_rejects_unsafe_api_base_overrides() {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));

    for api_base in [
        "http://example.com/api",
        "https://user@example.com/api",
        "https://example.com/api?debug=1",
        "https://example.com/api#frag",
    ] {
        let err = ExtConfig {
            app_token_secret: Some("app".to_owned()),
            bot_token_secret: Some("bot".to_owned()),
            allowed_user_ids: vec!["U123".to_owned()],
            api_base: Some(api_base.to_owned()),
            ..Default::default()
        }
        .validate(&secrets)
        .err()
        .expect("unsafe api base");
        assert!(err.contains("api_base"));
    }

    let cfg = ExtConfig {
        app_token_secret: Some("app".to_owned()),
        bot_token_secret: Some("bot".to_owned()),
        allowed_user_ids: vec!["U123".to_owned()],
        api_base: Some("http://127.0.0.1:8080/api".to_owned()),
        ..Default::default()
    }
    .validate(&secrets)
    .expect("loopback accepted");
    assert_eq!(cfg.api_base, "http://127.0.0.1:8080/api");
}

/// Once Socket Mode owns a config snapshot, reconfiguration must fail closed
/// and leave active credentials/routing untouched until Tau restarts.
#[test]
fn config_after_worker_start_is_rejected() {
    let (ext, _rx, _client) = extension();
    ext.state.lock().expect("lock").worker_started = true;
    let mut new_cfg = cfg();
    new_cfg.configured_channel_ids = ["C999".to_owned()].into_iter().collect();
    let err = ext.apply_config(new_cfg).expect_err("locked config");
    assert!(err.contains("restart Tau"));
    assert_eq!(
        ext.state
            .lock()
            .expect("lock")
            .config
            .as_ref()
            .map(|cfg| cfg.configured_channel_ids.contains("C123")),
        Some(true)
    );
}

/// Before worker startup, invalid reconfiguration clears inactive config and
/// registrations so stale credentials or destinations cannot remain live.
#[test]
fn invalid_pre_start_reconfiguration_clears_inactive_state() {
    let (ext, _rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.remember_posted_message(
        slack_conversation("C123", Some("10.0")),
        PostedMessage {
            ts: "1.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    ext.clear_config_after_error();
    let state = ext.state.lock().expect("lock");
    assert!(state.config.is_none());
    assert!(state.registered_agents.is_empty());
    assert!(
        state
            .posted_messages
            .get(&PostedMessageKey::new("C123", "1.0"))
            .is_none()
    );
}

/// Changing the configured channel set before startup clears post ownership
/// along with registrations so old destinations cannot survive reconfiguration.
#[test]
fn channel_reconfiguration_clears_post_ownership() {
    let (ext, _rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.remember_posted_message(
        slack_conversation("C123", Some("10.0")),
        PostedMessage {
            ts: "1.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    ext.apply_config(multi_channel_cfg()).expect("reconfigure");
    let state = ext.state.lock().expect("lock");
    assert!(state.registered_agents.is_empty());
    assert!(
        state
            .posted_messages
            .get(&PostedMessageKey::new("C123", "1.0"))
            .is_none()
    );
}

/// Protocol migration regression: a malformed pre-start config must emit a
/// parse `ConfigError`, clear inactive config, and prevent a later registration
/// from starting Slack with stale credentials.
#[test]
fn run_malformed_pre_start_config_clears_inactive_state() {
    let client = FakeClient::new();
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            malformed_config_message(),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-a",
                bool_args(true),
            ))),
        ],
        client.clone(),
    );

    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::ConfigError(error)
                if error.message.contains("unknown_field")
        )),
        "malformed config should be reported"
    );
    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::Emit(emit)
                if matches!(
                    emit.event.as_ref(),
                    Event::ToolError(error)
                        if error.tool_name.as_str() == REGISTER_TOOL_NAME
                            && error.message.contains("not configured")
                )
        )),
        "register should fail after malformed config clears active config"
    );
    assert_eq!(*client.auth_count.lock().expect("lock"), 0);
    assert_eq!(*client.open_count.lock().expect("lock"), 0);
}

/// Protocol migration regression: once Socket Mode has started, even malformed
/// config must return the immutable/restart-required error without clearing
/// active registration or Slack routing state.
#[test]
fn run_malformed_post_start_config_preserves_active_state() {
    let client = FakeClient::new();
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-a",
                bool_args(true),
            ))),
            malformed_config_message(),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                SEND_TOOL_NAME,
                "agent-a",
                message_args("reply"),
            ))),
        ],
        client.clone(),
    );

    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::ConfigError(error)
                if error.message.contains("cannot be changed after Socket Mode")
        )),
        "post-start malformed config should report immutable config"
    );
    assert!(
        !frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::ConfigError(error)
                if error.message.contains("unknown_field")
        )),
        "post-start config should not be parsed after worker startup"
    );
    assert!(client.sent.lock().expect("lock").is_empty());
    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Emit(emit)
            if matches!(
                emit.event.as_ref(),
                Event::ToolError(error)
                    if error.tool_name.as_str() == SEND_TOOL_NAME
                        && error.message.contains("originating conversation")
            )
    )));
}

/// Replayed lifecycle events are ignored wholesale by the tau-client migration,
/// so historical session shutdown cannot clear a live Slack registration.
#[test]
fn run_replayed_lifecycle_event_does_not_clear_registration() {
    let client = FakeClient::new();
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-a",
                bool_args(true),
            ))),
            HarnessOutputMessage::deliver_replay(
                tau_proto::UnixMicros::new(1_700_000_000_000_000),
                Event::SessionShutdown(tau_proto::SessionShutdown {
                    session_id: "s1".into(),
                }),
            ),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                SEND_TOOL_NAME,
                "agent-a",
                message_args("after replay"),
            ))),
        ],
        client.clone(),
    );

    assert!(client.sent.lock().expect("lock").is_empty());
    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Emit(emit)
            if matches!(
                emit.event.as_ref(),
                Event::ToolError(error)
                    if error.tool_name.as_str() == SEND_TOOL_NAME
                        && error.message.contains("originating conversation")
            )
    )));
}

/// Bad tool arguments should emit a tool error and return `Ok(())` from the
/// tau-client handler so the runner continues to handle subsequent Slack tools.
#[test]
fn run_bad_tool_args_do_not_stop_runner() {
    let client = FakeClient::new();
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                SEND_TOOL_NAME,
                "agent-a",
                CborValue::Map(vec![(
                    CborValue::Text("channel_id".to_owned()),
                    CborValue::Text("C999".to_owned()),
                )]),
            ))),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-a",
                bool_args(true),
            ))),
        ],
        client.clone(),
    );

    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::Emit(emit)
                if matches!(
                    emit.event.as_ref(),
                    Event::ToolError(error)
                        if error.tool_name.as_str() == SEND_TOOL_NAME
                            && error.message.contains("unknown argument")
                )
        )),
        "bad send args should emit ToolError"
    );
    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::Emit(emit)
                if matches!(
                    emit.event.as_ref(),
                    Event::ToolResult(result)
                        if result.tool_name.as_str() == REGISTER_TOOL_NAME
                )
        )),
        "runner should continue to later register tool"
    );
    assert_eq!(*client.auth_count.lock().expect("lock"), 1);
}

/// `slack_send` is available only after the calling agent registers, preventing
/// accidental replies from unrelated agents.
#[test]
fn slack_send_fails_before_register() {
    let (ext, _rx, _client) = extension();
    let event = ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("hi")));
    let Event::ToolError(err) = event else {
        panic!("expected error");
    };
    assert!(err.message.contains("slack_register"));
}

/// Registration records the calling agent and lazily starts Socket Mode;
/// turning it off removes selections pointing at the same agent.
#[test]
fn slack_register_toggles_agent_and_starts_worker() {
    let (ext, _rx, _client) = extension();
    let result = ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(true)));
    assert!(matches!(result, Event::ToolResult(_)));
    {
        let state = ext.state.lock().expect("lock");
        assert!(state.worker_started);
        assert!(state.registered_agents.contains(&agent_id("agent-a")));
    }
    ext.state
        .lock()
        .expect("lock")
        .selected_agent_by_channel
        .insert("C123".to_owned(), agent_id("agent-a"));
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            ts: "1.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let result = ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(false)));
    assert!(matches!(result, Event::ToolResult(_)));
    let state = ext.state.lock().expect("lock");
    assert!(!state.registered_agents.contains(&agent_id("agent-a")));
    assert!(state.selected_agent_by_channel.is_empty());
    assert!(
        state
            .posted_messages
            .get(&PostedMessageKey::new("C123", "1.0"))
            .is_none()
    );
}

/// Registration performs a bounded Slack auth/open preflight before reporting
/// success, so bad tokens are visible as tool errors instead of silent
/// background-only failures.
#[test]
fn slack_register_reports_initial_auth_failure() {
    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(Arc::new(FailingAuthClient), tx);
    ext.apply_config(cfg()).expect("config");
    let event = ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(true)));
    let Event::ToolError(err) = event else {
        panic!("expected tool error");
    };
    assert!(err.message.contains("invalid_auth"));
    assert!(ext.state.lock().expect("lock").registered_agents.is_empty());
}

/// Registered agents reply only to the configured conversation from which
/// their most recent Slack prompt originated.
#[test]
fn slack_send_uses_originating_conversation() {
    let (ext, rx, client) = extension();
    ext.apply_config(multi_channel_cfg())
        .expect("multi-channel config");
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C456", None, "<@UBOT123> hello"));
    let prompt = recv_prompt_request(&rx);
    assert_eq!(prompt.text, "[slack from U123] hello");
    activate_prompt_origin(&ext, &prompt);
    let result = ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("hello")));
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        client.sent_pairs(),
        vec![("C456".to_owned(), "[agent-a] hello".to_owned())]
    );
}

/// Root messages keep replies top-level while thread messages automatically
/// carry their originating root without any model-supplied destination.
#[test]
fn slack_send_preserves_root_and_thread_context() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");

    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> root"));
    let root = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &root);
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("root reply"))),
        Event::ToolResult(_)
    ));

    let mut threaded = slack_message("C123", None, "<@UBOT123> threaded");
    threaded.thread_ts = Some("42.0".to_owned());
    ext.process_slack_message(threaded);
    let threaded = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &threaded);
    assert!(matches!(
        ext.handle_send(tool(
            SEND_TOOL_NAME,
            "agent-a",
            message_args("thread reply")
        )),
        Event::ToolResult(_)
    ));

    assert_eq!(
        client.sent_thread_ids(),
        vec![None, Some("42.0".to_owned())]
    );
}

/// Authorized reactions to an agent-owned bridge post route to that same agent
/// with stable source metadata and duplicate retries are suppressed.
#[test]
fn authorized_reactions_to_agent_posts_are_routed_and_deduplicated() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> question"));
    let prompt = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &prompt);
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("answer"))),
        Event::ToolResult(_)
    ));
    let reaction = slack_reaction("ER1", "reaction_added", "C123", "1.0");
    let mut reaction = reaction;
    reaction.reaction = "thumbsup::skin-tone-6".to_owned();
    ext.process_slack_reaction(reaction);
    let prompt = recv_prompt_request(&rx);
    assert_eq!(prompt.agent_id, agent_id("agent-a"));
    assert_eq!(
        prompt.text,
        "[slack reaction type=reaction_added user=U123 channel=C123 message_ts=1.0 thread_ts=- reaction=thumbsup::skin-tone-6]"
    );
    ext.process_slack_reaction(slack_reaction("ER1", "reaction_added", "C123", "1.0"));
    assert!(rx.try_recv().is_err());
}

/// Canonical Slack skin-tone reactions are accepted, while malformed colon
/// suffixes and unsafe timestamp metadata fail closed.
#[test]
fn reaction_metadata_validation_matches_slack_grammar() {
    for valid in ["thumbsup", "thumbsup::skin-tone-2", "wave::skin-tone-6"] {
        assert!(validate_reaction_name(valid).is_ok(), "{valid}");
    }
    for invalid in [
        "",
        "thumbsup:",
        "thumbsup::",
        "thumbsup::skin-tone-1",
        "thumbsup::skin-tone-7",
        "thumbsup::skin-tone-2::extra",
        "bad reaction",
    ] {
        assert!(validate_reaction_name(invalid).is_err(), "{invalid}");
    }
    for valid in ["1.0", "1712345678.123456"] {
        assert!(validate_slack_ts(valid).is_ok(), "{valid}");
    }
    for invalid in ["", "1", ".1", "1.", "1.2.3", "x.1", "1. x"] {
        assert!(validate_slack_ts(invalid).is_err(), "{invalid}");
    }
}

/// Bot users remain unable to route reactions even if an operator mistakenly
/// includes their U-shaped bot user id in the allowlist.
#[test]
fn allowlisted_non_human_reaction_actor_is_rejected() {
    let (ext, rx, _client) = extension();
    {
        let mut state = ext.state.lock().expect("lock");
        state
            .config
            .as_mut()
            .expect("config")
            .allowed_user_ids
            .insert("UBOT999".to_owned());
    }
    register_agent(&ext, "agent-a");
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            ts: "1.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let mut reaction = slack_reaction("ER-BOT", "reaction_added", "C123", "1.0");
    reaction.user_id = "UBOT999".to_owned();
    ext.process_slack_reaction(reaction);
    assert!(rx.try_recv().is_err());
}

/// Slack post response parsing requires a canonical message timestamp and
/// retains only canonical optional thread metadata.
#[test]
fn posted_message_response_validates_identity_metadata() {
    assert!(posted_message_from_response(&serde_json::json!({})).is_err());
    assert!(posted_message_from_response(&serde_json::json!({ "ts": "bad" })).is_err());
    let post = posted_message_from_response(&serde_json::json!({
        "ts": "12.34",
        "message": { "thread_ts": "not-a-ts" }
    }))
    .expect("valid message ts");
    assert_eq!(post.ts, "12.34");
    assert!(post.thread_ts.is_none());
}

/// Web API request serialization omits thread metadata for root posts and
/// includes a supplied validated root for threaded posts.
#[test]
fn post_message_body_serializes_optional_thread_context() {
    assert_eq!(
        post_message_body("C123", "root", None),
        serde_json::json!({ "channel": "C123", "text": "root" })
    );
    assert_eq!(
        post_message_body("C123", "reply", Some("42.0")),
        serde_json::json!({
            "channel": "C123",
            "text": "reply",
            "thread_ts": "42.0"
        })
    );
}

/// Eviction, agent removal, and clear keep semantic post ownership synchronized
/// so stale message identities cannot reappear.
#[test]
fn posted_message_cache_eviction_and_cleanup_are_synchronized() {
    let agent_a = agent_id("agent-a");
    let agent_b = agent_id("agent-b");
    let mut cache = PostedMessageCache::new(2);
    for (ts, agent_id) in [
        ("1.0", agent_a.clone()),
        ("2.0", agent_b.clone()),
        ("3.0", agent_a.clone()),
    ] {
        cache.insert(
            PostedMessageKey::new("C123", ts),
            PostedMessageOwner {
                agent_id,
                thread_ts: None,
            },
        );
    }
    assert!(cache.get(&PostedMessageKey::new("C123", "1.0")).is_none());
    assert!(cache.get(&PostedMessageKey::new("C123", "2.0")).is_some());
    assert!(cache.get(&PostedMessageKey::new("C123", "3.0")).is_some());
    cache.remove_agent(&agent_a);
    assert!(cache.get(&PostedMessageKey::new("C123", "2.0")).is_some());
    assert!(cache.get(&PostedMessageKey::new("C123", "3.0")).is_none());
    cache.clear();
    assert!(cache.get(&PostedMessageKey::new("C123", "2.0")).is_none());
}

/// Human verification fails closed when Slack omits account-type facts and
/// rejects deleted, bot, and app-user accounts.
#[test]
fn users_info_response_requires_explicit_live_human_facts() {
    assert!(human_user_from_response(&serde_json::json!({}), "U123").is_err());
    assert!(!human_user_from_response(&serde_json::json!({ "user": {} }), "U123").expect("shape"));
    assert!(
        human_user_from_response(
            &serde_json::json!({
                "user": { "id": "U123", "deleted": false, "is_bot": false, "is_app_user": false }
            }),
            "U123"
        )
        .expect("human")
    );
    for user in [
        serde_json::json!({ "id": "U123", "deleted": true, "is_bot": false }),
        serde_json::json!({ "id": "U123", "deleted": false, "is_bot": true }),
        serde_json::json!({ "id": "U123", "deleted": false, "is_bot": false, "is_app_user": true }),
        serde_json::json!({ "id": "U999", "deleted": false, "is_bot": false }),
        serde_json::json!({ "id": "U123", "deleted": "false", "is_bot": false }),
    ] {
        assert!(
            !human_user_from_response(&serde_json::json!({ "user": user }), "U123")
                .expect("account")
        );
    }
    assert!(
        !human_user_from_response(
            &serde_json::json!({
                "user": { "id": "USLACKBOT", "deleted": false, "is_bot": false }
            }),
            "USLACKBOT"
        )
        .expect("slackbot")
    );
}

/// Reactions from unauthorized users, unconfigured conversations, or messages
/// not posted by this bridge never become prompts.
#[test]
fn reactions_outside_authorized_owned_posts_are_ignored() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            ts: "1.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let mut unauthorized = slack_reaction("ER1", "reaction_added", "C123", "1.0");
    unauthorized.user_id = "U999".to_owned();
    ext.process_slack_reaction(unauthorized);
    ext.process_slack_reaction(slack_reaction("ER2", "reaction_added", "C999", "1.0"));
    ext.process_slack_reaction(slack_reaction("ER3", "reaction_removed", "C123", "404.0"));
    assert!(rx.try_recv().is_err());
}

/// Reaction prompts use the authenticated thread from the original outbound
/// request even when Slack omits it in the response, and conflicting event
/// metadata can never redirect a root or threaded post.
#[test]
fn reaction_routing_uses_cached_authenticated_thread_only() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    ext.remember_posted_message(
        slack_conversation("C123", Some("10.0")),
        PostedMessage {
            ts: "1.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let mut reaction = slack_reaction("ER-THREAD", "reaction_added", "C123", "1.0");
    reaction.thread_ts = None;
    ext.process_slack_reaction(reaction);
    let prompt = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &prompt);
    assert!(matches!(
        ext.handle_send(tool(
            SEND_TOOL_NAME,
            "agent-a",
            message_args("thread reply")
        )),
        Event::ToolResult(_)
    ));
    assert_eq!(
        client.sent_thread_ids().last(),
        Some(&Some("10.0".to_owned()))
    );

    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            ts: "2.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let mut root_conflict = slack_reaction("ER-ROOT-CONFLICT", "reaction_added", "C123", "2.0");
    root_conflict.thread_ts = Some("99.0".to_owned());
    ext.process_slack_reaction(root_conflict);

    ext.remember_posted_message(
        slack_conversation("C123", Some("10.0")),
        PostedMessage {
            ts: "3.0".to_owned(),
            thread_ts: Some("10.0".to_owned()),
        },
        agent_id("agent-a"),
    );
    let mut thread_conflict = slack_reaction("ER-THREAD-CONFLICT", "reaction_added", "C123", "3.0");
    thread_conflict.thread_ts = Some("99.0".to_owned());
    ext.process_slack_reaction(thread_conflict);

    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            ts: "4.0".to_owned(),
            thread_ts: Some("99.0".to_owned()),
        },
        agent_id("agent-a"),
    );
    ext.process_slack_reaction(slack_reaction(
        "ER-RESPONSE-CONFLICT",
        "reaction_added",
        "C123",
        "4.0",
    ));
    assert!(rx.try_recv().is_err());
}

/// A registered agent cannot send proactively merely because channels are
/// configured; an authorized inbound route must establish the destination.
#[test]
fn slack_send_rejects_missing_or_forged_origin_context() {
    let (ext, _rx, client) = extension();
    register_agent(&ext, "agent-a");
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("hello"))),
        Event::ToolError(_)
    ));
    ext.state
        .lock()
        .expect("lock")
        .outbound_conversation_by_agent
        .insert(
            agent_id("agent-a"),
            SlackConversation {
                channel_id: "C999".to_owned(),
                thread_ts: Some("9.0".to_owned()),
            },
        );
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("hello"))),
        Event::ToolError(_)
    ));
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Blank and oversized outbound messages are rejected before Slack API calls so
/// tool diagnostics remain deterministic and bounded.
#[test]
fn slack_send_rejects_blank_and_oversized_messages() {
    let (ext, _rx, client) = extension();
    register_agent(&ext, "agent-a");
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("   "))),
        Event::ToolError(_)
    ));
    let long = "x".repeat(DEFAULT_MAX_MESSAGE_BYTES + 1);
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args(&long))),
        Event::ToolError(_)
    ));
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Messages from users outside the allowlist are ignored with no prompt and no
/// Slack reply side effects.
#[test]
fn unallowed_user_produces_no_prompt_or_reply() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let mut msg = slack_message("C123", None, "<@UBOT123> hello");
    msg.user_id = "U999".to_owned();
    ext.process_slack_message(msg);
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Configured channel policy silently rejects other channels and DMs without
/// even granting them a Slack reply side effect.
#[test]
fn configured_channel_rejects_other_conversations() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C999", None, "<@UBOT123> hello"));
    ext.process_slack_message(slack_message("D123", Some("im"), "hello"));
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Each configured channel keeps its own selected Tau agent, so commands in
/// one shared Slack conversation cannot redirect another conversation.
#[test]
fn configured_channels_keep_independent_agent_selections() {
    let (ext, rx, _client) = extension();
    ext.apply_config(multi_channel_cfg())
        .expect("multi-channel config");
    register_agent(&ext, "agent-alpha");
    register_agent(&ext, "agent-beta");

    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> select agent-alpha"));
    ext.process_slack_message(slack_message("C456", None, "<@UBOT123> select agent-beta"));
    let mut first_message = slack_message("C123", None, "<@UBOT123> first");
    first_message.thread_ts = Some("10.0".to_owned());
    let mut second_message = slack_message("C456", None, "<@UBOT123> second");
    second_message.thread_ts = Some("20.0".to_owned());
    ext.process_slack_message(first_message);
    ext.process_slack_message(second_message);

    let first = recv_prompt_request(&rx);
    let second = recv_prompt_request(&rx);
    assert_eq!(first.agent_id, agent_id("agent-alpha"));
    assert_eq!(first.text, "[slack from U123] first");
    assert_eq!(second.agent_id, agent_id("agent-beta"));
    assert_eq!(second.text, "[slack from U123] second");
}

/// In DM-link mode, text before `start` receives guidance and the first linked
/// DM remains exclusive until restart or reconfiguration.
#[test]
fn dm_linking_is_explicit_and_exclusive() {
    let (ext, rx, client) = extension();
    ext.apply_config(dm_cfg()).expect("dm config");
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("D123", Some("im"), "hello before start"));
    assert!(rx.try_recv().is_err());
    assert!(
        client.sent.lock().expect("lock")[0]
            .text
            .contains("Send start")
    );

    ext.process_slack_message(slack_message("D123", Some("im"), "start"));
    assert_eq!(
        ext.state
            .lock()
            .expect("lock")
            .learned_dm
            .as_ref()
            .map(|link| link.channel_id.as_str()),
        Some("D123")
    );
    ext.process_slack_message(slack_message("D999", Some("im"), "start"));
    assert_eq!(
        ext.state
            .lock()
            .expect("lock")
            .learned_dm
            .as_ref()
            .map(|link| link.channel_id.as_str()),
        Some("D123")
    );
}

/// DM mode keeps the existing explicit `start` link and derives outbound
/// replies from later prompts routed through that allowlisted DM.
#[test]
fn dm_send_uses_linked_prompt_origin() {
    let (ext, rx, client) = extension();
    ext.apply_config(dm_cfg()).expect("dm config");
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("D123", Some("im"), "start"));
    let mut question = slack_message("D123", Some("im"), "question");
    question.thread_ts = Some("7.0".to_owned());
    ext.process_slack_message(question);
    let prompt = recv_prompt_request(&rx);
    assert_eq!(prompt.text, "[slack from U123] question");
    activate_prompt_origin(&ext, &prompt);
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("answer"))),
        Event::ToolResult(_)
    ));
    assert_eq!(
        client.sent_pairs().last(),
        Some(&("D123".to_owned(), "[agent-a] answer".to_owned()))
    );
    assert_eq!(
        client.sent_thread_ids().last(),
        Some(&Some("7.0".to_owned()))
    );
}

/// Queued Slack prompts cannot redirect an in-flight reply: authorization
/// changes only when the harness starts the exact correlated prompt.
#[test]
fn prompt_start_correlation_preserves_queued_origin_routing() {
    let (ext, rx, client) = extension();
    ext.apply_config(multi_channel_cfg())
        .expect("multi-channel config");
    register_agent(&ext, "agent-a");
    let mut first_message = slack_message("C123", None, "<@UBOT123> first");
    first_message.thread_ts = Some("10.0".to_owned());
    let mut second_message = slack_message("C456", None, "<@UBOT123> second");
    second_message.thread_ts = Some("20.0".to_owned());
    ext.process_slack_message(first_message);
    ext.process_slack_message(second_message);
    let first = recv_prompt_request(&rx);
    let second = recv_prompt_request(&rx);

    activate_prompt_origin(&ext, &first);
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("one"))),
        Event::ToolResult(_)
    ));
    activate_prompt_origin(&ext, &second);
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("two"))),
        Event::ToolResult(_)
    ));
    assert_eq!(
        client.sent_pairs(),
        vec![
            ("C123".to_owned(), "[agent-a] one".to_owned()),
            ("C456".to_owned(), "[agent-a] two".to_owned()),
        ]
    );
    assert_eq!(
        client.sent_thread_ids(),
        vec![Some("10.0".to_owned()), Some("20.0".to_owned())]
    );

    ext.handle_prompt_submitted(&tau_proto::AgentPromptSubmitted {
        agent_id: agent_id("agent-a"),
        text: "local prompt".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    });
    ext.handle_prompt_started(&agent_id("agent-a"), None);
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("local"))),
        Event::ToolError(_)
    ));
}

/// Busy-agent prompts authenticate and retire through the real queued/steered
/// lifecycle, but always revoke sending because the follow-up mixes origins.
#[test]
fn steered_prompt_origins_revoke_send_and_reclaim_correlations() {
    let (ext, rx, client) = extension();
    ext.apply_config(multi_channel_cfg())
        .expect("multi-channel config");
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C456", None, "<@UBOT123> queued"));
    let queued = recv_prompt_request(&rx);
    steer_prompt_origin(&ext, &queued);
    ext.handle_prompt_started(&agent_id("agent-a"), None);
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("reply"))),
        Event::ToolError(_)
    ));
    assert!(client.sent.lock().expect("lock").is_empty());
    assert!(
        ext.state
            .lock()
            .expect("lock")
            .pending_origin_by_ctx
            .is_empty()
    );

    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> first mixed"));
    ext.process_slack_message(slack_message("C456", None, "<@UBOT123> second mixed"));
    steer_prompt_origin(&ext, &recv_prompt_request(&rx));
    steer_prompt_origin(&ext, &recv_prompt_request(&rx));
    ext.handle_prompt_started(&agent_id("agent-a"), None);
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("ambiguous"))),
        Event::ToolError(_)
    ));
    assert!(
        ext.state
            .lock()
            .expect("lock")
            .pending_origin_by_ctx
            .is_empty()
    );
}

/// Recalling a queued Slack prompt retires its pending correlation so repeated
/// queue/recall cycles cannot exhaust the bridge's bounded route state.
#[test]
fn recalled_prompt_reclaims_pending_correlation() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> recall me"));
    let prompt = recv_prompt_request(&rx);
    assert_eq!(
        ext.state.lock().expect("lock").pending_origin_by_ctx.len(),
        1
    );
    ext.handle_prompt_recalled(&tau_proto::AgentPromptRecalled {
        agent_id: prompt.agent_id,
        text: prompt.text,
    });
    assert!(
        ext.state
            .lock()
            .expect("lock")
            .pending_origin_by_ctx
            .is_empty()
    );
}

/// Context-less tool-result follow-up starts preserve a previously activated
/// Slack origin only when no submitted, steered, or recalled prompt intervenes.
#[test]
fn contextless_followup_preserves_uncontaminated_origin() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let mut message = slack_message("C123", None, "<@UBOT123> investigate");
    message.thread_ts = Some("33.0".to_owned());
    ext.process_slack_message(message);
    let prompt = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &prompt);
    ext.handle_prompt_started(&agent_id("agent-a"), None);
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("done"))),
        Event::ToolResult(_)
    ));
    assert_eq!(client.sent.lock().expect("lock").len(), 1);
    assert_eq!(
        client.sent_thread_ids().last(),
        Some(&Some("33.0".to_owned()))
    );
}

/// The registered tau-client live handlers must dispatch every Slack
/// authorization lifecycle event while replayed lifecycle facts remain inert.
#[test]
fn protocol_lifecycle_handlers_dispatch_live_and_ignore_replay() {
    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    ext.apply_config(multi_channel_cfg()).expect("config");
    register_agent(&ext, "agent-a");
    {
        let mut state = ext.state.lock().expect("lock");
        state.outbound_conversation_by_agent.insert(
            agent_id("agent-a"),
            SlackConversation {
                channel_id: "C123".to_owned(),
                thread_ts: None,
            },
        );
        for (ctx, channel, text) in [
            ("ctx-live", "C456", "live prompt"),
            ("ctx-replay", "C456", "replayed prompt"),
            ("ctx-steer", "C456", "steered prompt"),
            ("ctx-recall", "C456", "recalled prompt"),
        ] {
            state.pending_origin_by_ctx.insert(
                ctx.to_owned(),
                PendingOrigin {
                    agent_id: agent_id("agent-a"),
                    conversation: SlackConversation {
                        channel_id: channel.to_owned(),
                        thread_ts: None,
                    },
                    prompt: text.to_owned(),
                },
            );
        }
    }
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = tau_client::TauExtensionRunner::new(SlackExtension)
        .start_manual_loop(
            std::io::Cursor::new(Vec::new()),
            writer,
            SlackRuntime { ext },
        )
        .expect("manual runtime");
    let submitted = |ctx: &str, text: &str| {
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: agent_id("agent-a"),
            text: text.to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: Some(ctx.to_owned()),
        })
    };
    runtime
        .dispatch_one(HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1),
            submitted("ctx-replay", "replayed prompt"),
        ))
        .expect("replayed submitted");
    runtime
        .dispatch_one(HarnessOutputMessage::deliver_live(
            tau_proto::UnixMicros::new(2),
            submitted("ctx-live", "live prompt"),
        ))
        .expect("live submitted");
    runtime
        .dispatch_one(HarnessOutputMessage::deliver_live(
            tau_proto::UnixMicros::new(3),
            Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
                agent_prompt_id: "sp1".into(),
                agent_id: agent_id("agent-a"),
                session_id: "s1".into(),
                model: "test/model".parse().expect("model"),
                originator: tau_proto::PromptOriginator::User,
                ctx_id: Some("ctx-live".to_owned()),
            }),
        ))
        .expect("live started");
    runtime
        .dispatch_one(HarnessOutputMessage::deliver_live(
            tau_proto::UnixMicros::new(4),
            Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                agent_id: agent_id("agent-a"),
                text: "steered prompt".to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                ctx_id: Some("ctx-steer".to_owned()),
            }),
        ))
        .expect("live steered");
    runtime
        .dispatch_one(HarnessOutputMessage::deliver_live(
            tau_proto::UnixMicros::new(5),
            Event::AgentPromptRecalled(tau_proto::AgentPromptRecalled {
                agent_id: agent_id("agent-a"),
                text: "recalled prompt".to_owned(),
            }),
        ))
        .expect("live recalled");
    let state = runtime.finish().expect("finish").ext.state.clone();
    let state = state.lock().expect("lock");
    assert!(state.outbound_conversation_by_agent.is_empty());
    assert!(state.pending_origin_by_ctx.contains_key("ctx-replay"));
    assert!(!state.pending_origin_by_ctx.contains_key("ctx-live"));
    assert!(!state.pending_origin_by_ctx.contains_key("ctx-steer"));
    assert!(!state.pending_origin_by_ctx.contains_key("ctx-recall"));
    drop(state);
    let mut reader = tau_proto::HarnessInputReader::new(std::io::Cursor::new(written.bytes()));
    let mut subscribe = None;
    while let Some(frame) = reader.read_message().expect("startup frame") {
        if let HarnessInputMessage::Subscribe(value) = frame {
            subscribe = Some(value);
        }
    }
    let subscribe = subscribe.expect("subscribe frame");
    for event_name in [
        tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
        tau_proto::EventName::AGENT_PROMPT_STARTED,
        tau_proto::EventName::AGENT_PROMPT_STEERED,
        tau_proto::EventName::AGENT_PROMPT_RECALLED,
    ] {
        let selector = tau_proto::EventSelector::Exact(event_name);
        assert!(subscribe.live_selectors.contains(&selector));
        assert!(!subscribe.historical_selectors.contains(&selector));
    }
}

/// Live unload and shutdown handlers remove cached post ownership so later
/// reactions cannot target agents after their lifecycle ends.
#[test]
fn live_lifecycle_handlers_clear_post_ownership() {
    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    ext.apply_config(cfg()).expect("config");
    register_agent(&ext, "agent-a");
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            ts: "1.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let mut runtime = tau_client::TauExtensionRunner::new(SlackExtension)
        .start_manual_loop(
            std::io::Cursor::new(Vec::new()),
            SharedWriter::default(),
            SlackRuntime { ext },
        )
        .expect("runtime");
    runtime
        .dispatch_one(HarnessOutputMessage::deliver_live(
            tau_proto::UnixMicros::new(1),
            Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                session_id: "s1".into(),
                agent_id: agent_id("agent-a"),
            }),
        ))
        .expect("unload");
    assert!(
        runtime
            .state()
            .ext
            .state
            .lock()
            .expect("lock")
            .posted_messages
            .get(&PostedMessageKey::new("C123", "1.0"))
            .is_none()
    );

    register_agent(&runtime.state().ext, "agent-a");
    runtime.state().ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            ts: "2.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    runtime
        .dispatch_one(HarnessOutputMessage::deliver_live(
            tau_proto::UnixMicros::new(2),
            Event::SessionShutdown(tau_proto::SessionShutdown {
                session_id: "s1".into(),
            }),
        ))
        .expect("shutdown");
    let state = runtime.finish().expect("finish").ext.state.clone();
    assert!(
        state
            .lock()
            .expect("lock")
            .posted_messages
            .get(&PostedMessageKey::new("C123", "2.0"))
            .is_none()
    );
}

/// Forged lifecycle facts with mismatched text or agent cannot activate a
/// pending Slack destination and consume the pending record fail closed.
#[test]
fn mismatched_prompt_lifecycle_cannot_authorize_send() {
    for (submitted_agent, submitted_text, submitted_ctx) in [
        ("agent-a", "forged", None),
        ("agent-b", "[slack from U123] genuine", None),
        ("agent-a", "[slack from U123] genuine", Some("wrong-ctx")),
    ] {
        let (ext, rx, client) = extension();
        register_agent(&ext, "agent-a");
        ext.process_slack_message(slack_message("C123", None, "<@UBOT123> genuine"));
        let prompt = recv_prompt_request(&rx);
        ext.handle_prompt_submitted(&tau_proto::AgentPromptSubmitted {
            agent_id: agent_id(submitted_agent),
            text: submitted_text.to_owned(),
            message_class: prompt.message_class,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: submitted_ctx
                .map(str::to_owned)
                .or_else(|| prompt.ctx_id.clone()),
        });
        ext.handle_prompt_started(&prompt.agent_id, prompt.ctx_id.as_deref());
        assert!(matches!(
            ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("no"))),
            Event::ToolError(_)
        ));
        assert!(client.sent.lock().expect("lock").is_empty());
    }
}

/// Pending and accepted correlations share one hard capacity so moving records
/// between lifecycle stages cannot bypass the memory bound.
#[test]
fn route_correlation_limit_counts_pending_and_accepted_together() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    {
        let mut state = ext.state.lock().expect("lock");
        for index in 0..ROUTE_CORRELATION_LIMIT - 1 {
            state.accepted_origin_by_ctx.insert(
                format!("accepted:{index}"),
                AcceptedOrigin {
                    agent_id: agent_id("agent-a"),
                    conversation: SlackConversation {
                        channel_id: "C123".to_owned(),
                        thread_ts: None,
                    },
                },
            );
        }
    }
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> last slot"));
    let _ = recv_prompt_request(&rx);
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> over limit"));
    assert!(rx.try_recv().is_err());
    assert!(
        client
            .sent
            .lock()
            .expect("lock")
            .last()
            .is_some_and(|message| message.text.contains("too many pending"))
    );
}

/// With exactly one registered agent, plain Slack text becomes a normal
/// external prompt request with compact sanitized source context.
#[test]
fn one_registered_agent_receives_plain_text() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> hello"));
    assert_eq!(recv_prompt(&rx), "[slack from U123] hello");
}

/// Multiple registered agents without a selection produce Slack guidance rather
/// than guessing which Tau agent should receive prompt text.
#[test]
fn multiple_agents_without_selection_get_guidance() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    register_agent(&ext, "agent-b");
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> hello"));
    assert!(rx.try_recv().is_err());
    assert!(
        client.sent.lock().expect("lock")[0]
            .text
            .contains("Multiple Tau agents")
    );
}

/// `agents`, `select`, and `to` use stable agent ids in Slack-visible routing
/// commands while display names remain parenthetical context only.
#[test]
fn agents_select_and_to_route_by_agent_id() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-alpha");
    register_agent(&ext, "agent-beta");
    ext.state
        .lock()
        .expect("lock")
        .agent_labels
        .insert(agent_id("agent-alpha"), "Alpha Display".to_owned());

    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> agents"));
    assert!(
        client.sent.lock().expect("lock")[0]
            .text
            .contains("agent-alpha (Alpha Display)")
    );

    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> select agent-al"));
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> later"));
    assert_eq!(recv_prompt(&rx), "[slack from U123] later");

    ext.process_slack_message(slack_message(
        "C123",
        None,
        "<@UBOT123> to agent-beta direct",
    ));
    assert_eq!(recv_prompt(&rx), "[slack from U123] direct");
}

/// Malformed command-shaped text is handled as command feedback and must not
/// fall through as a routed prompt.
#[test]
fn malformed_commands_do_not_become_prompts() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let mut select = slack_message("C123", None, "<@UBOT123> select");
    select.thread_ts = Some("44.0".to_owned());
    let mut unknown = slack_message("C123", None, "<@UBOT123> /unknown");
    unknown.thread_ts = Some("44.0".to_owned());
    ext.process_slack_message(select);
    ext.process_slack_message(unknown);
    assert!(rx.try_recv().is_err());
    assert_eq!(client.sent.lock().expect("lock").len(), 2);
    assert_eq!(
        client.sent_thread_ids(),
        vec![Some("44.0".to_owned()), Some("44.0".to_owned())]
    );
}

/// Socket Mode envelopes with ids are acked before routing, so Slack retries
/// are avoided while prompt submission still happens through the extension
/// event.
#[test]
fn valid_envelopes_are_acked_and_routed() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let text = serde_json::json!({
        "type": "events_api",
        "envelope_id": "env-1",
        "payload": {
            "type": "event_callback",
            "event_id": "Ev1",
            "event": {
                "type": "app_mention",
                "channel": "C123",
                "user": "U123",
                "text": "<@UBOT123> hello",
                "ts": "1.0",
                "thread_ts": "0.5"
            }
        }
    })
    .to_string();
    let action = handle_socket_text(&ext, &text);
    assert_eq!(action.ack_envelope_id.as_deref(), Some("env-1"));
    let Some(DecodedSlackEvent::Message(message)) = action.event else {
        panic!("decoded message");
    };
    assert_eq!(message.thread_ts.as_deref(), Some("0.5"));
    ext.process_slack_message(message);
    assert_eq!(recv_prompt(&rx), "[slack from U123] hello");
}

/// Malformed thread metadata is rejected rather than downgraded to a top-level
/// destination that could misroute a reply.
#[test]
fn malformed_message_thread_metadata_is_rejected() {
    let value = serde_json::json!({
        "payload": {
            "type": "event_callback",
            "event_id": "Ev-thread-bad",
            "event": {
                "type": "app_mention",
                "channel": "C123",
                "user": "U123",
                "text": "<@UBOT123> hello",
                "ts": "1.0",
                "thread_ts": "not-a-ts"
            }
        }
    });
    assert!(decode_socket_event(&value).is_none());
}

/// Socket Mode reaction envelopes retain stable message identity metadata and
/// are acked independently of later authorization checks.
#[test]
fn reaction_envelopes_are_acked_and_decoded() {
    let (ext, _rx, _client) = extension();
    let text = serde_json::json!({
        "type": "events_api",
        "envelope_id": "env-reaction",
        "payload": {
            "type": "event_callback",
            "event_id": "Er1",
            "event": {
                "type": "reaction_removed",
                "user": "U123",
                "reaction": "eyes",
                "item": {
                    "type": "message",
                    "channel": "C123",
                    "ts": "12.34",
                    "thread_ts": "10.00"
                }
            }
        }
    })
    .to_string();
    let action = handle_socket_text(&ext, &text);
    assert_eq!(action.ack_envelope_id.as_deref(), Some("env-reaction"));
    let Some(DecodedSlackEvent::Reaction(reaction)) = action.event else {
        panic!("decoded reaction");
    };
    assert_eq!(reaction.event_type.as_str(), "reaction_removed");
    assert_eq!(reaction.channel_id, "C123");
    assert_eq!(reaction.message_ts, "12.34");
    assert_eq!(reaction.thread_ts.as_deref(), Some("10.00"));
}

/// Slack event types are conversation-specific: configured channels accept
/// mentions, while DM mode accepts only direct-message `message` events.
#[test]
fn mention_and_message_event_types_do_not_cross_conversation_modes() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let mut channel_message = slack_message("C123", None, "<@UBOT123> channel");
    channel_message.event_type = "message".to_owned();
    ext.process_slack_message(channel_message);
    let mut dm_mention = slack_message("D123", Some("im"), "<@UBOT123> dm");
    dm_mention.event_type = "app_mention".to_owned();
    ext.process_slack_message(dm_mention);
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Slack retries and reconnect replays with the same event id are dropped so
/// Tau does not receive duplicate external prompts.
#[test]
fn duplicate_slack_event_ids_are_dropped() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let msg = slack_message("C123", None, "<@UBOT123> hello");
    ext.process_slack_message(msg.clone());
    ext.process_slack_message(msg);
    assert_eq!(recv_prompt(&rx), "[slack from U123] hello");
    assert!(rx.try_recv().is_err());
}

/// Bot/self messages and message subtypes are ignored to avoid routing Slack
/// bot echoes, edits, joins, deletes, or other non-user text events.
#[test]
fn bot_self_and_subtype_messages_are_ignored() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let mut self_msg = slack_message("C123", None, "<@UBOT123> echo");
    self_msg.user_id = "UBOT123".to_owned();
    ext.process_slack_message(self_msg);
    let mut bot_msg = slack_message("C123", None, "<@UBOT123> bot");
    bot_msg.bot_id = Some("B123".to_owned());
    ext.process_slack_message(bot_msg);
    let mut edit = slack_message("C123", None, "<@UBOT123> edit");
    edit.subtype = Some("message_changed".to_owned());
    ext.process_slack_message(edit);
    assert!(rx.try_recv().is_err());
}

/// Slack app/bot tokens and Socket Mode URLs are redacted or rejected in
/// diagnostics that may become log-visible or model-visible tool errors.
#[test]
fn token_and_socket_diagnostics_are_sanitized() {
    let cfg = cfg();
    let text = sanitize_diagnostic(
        "xapp-test xoxb-test wss://wss-primary.slack.com/link?ticket=secret",
        &cfg,
    );
    assert!(!text.contains("xapp-test"));
    assert!(!text.contains("xoxb-test"));
    let socket_url = format!(
        "wss://wss-primary.slack.com/link?ticket={}",
        "secret-ticket".repeat(80)
    );
    let socket_error = sanitize_socket_diagnostic(
        &format!("connect failed for {socket_url}"),
        &cfg,
        &socket_url,
    );
    assert!(!socket_error.contains("wss://wss-primary.slack.com"));
    assert!(!socket_error.contains("secret-ticket"));
    assert!(socket_error.len() <= MAX_DIAGNOSTIC_BYTES + 3);
    assert!(validate_socket_url("ws://example.com/socket?ticket=secret").is_err());
    assert!(validate_socket_url("ws://127.0.0.1:9000/socket?ticket=secret").is_ok());
}
