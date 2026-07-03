use std::sync::Mutex;

use tau_proto::{HarnessInputMessage, ToolStarted};

use super::*;

struct FakeClient {
    sent: Mutex<Vec<(String, String)>>,
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

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        channel_id: &str,
        text: &str,
    ) -> Result<(), String> {
        self.sent
            .lock()
            .expect("lock")
            .push((channel_id.to_owned(), text.to_owned()));
        Ok(())
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

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        _channel_id: &str,
        _text: &str,
    ) -> Result<(), String> {
        Ok(())
    }
}

fn cfg() -> RuntimeConfig {
    RuntimeConfig {
        app_token: "xapp-test".to_owned(),
        bot_token: "xoxb-test".to_owned(),
        allowed_user_ids: ["U123".to_owned()].into_iter().collect(),
        configured_channel_id: Some("C123".to_owned()),
        api_base: DEFAULT_API_BASE.to_owned(),
        max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
    }
}

fn dm_cfg() -> RuntimeConfig {
    RuntimeConfig {
        configured_channel_id: None,
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
    let err = tau_extension::parse_config::<ExtConfig>(&value).expect_err("unknown field");
    assert!(err.contains("unknown field"));
    assert!(err.contains("destination"));
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
    new_cfg.configured_channel_id = Some("C999".to_owned());
    let err = ext.apply_config(new_cfg).expect_err("locked config");
    assert!(err.contains("restart Tau"));
    assert_eq!(
        ext.state
            .lock()
            .expect("lock")
            .config
            .as_ref()
            .and_then(|cfg| cfg.configured_channel_id.as_deref()),
        Some("C123")
    );
}

/// Before worker startup, invalid reconfiguration clears inactive config and
/// registrations so stale credentials or destinations cannot remain live.
#[test]
fn invalid_pre_start_reconfiguration_clears_inactive_state() {
    let (ext, _rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.clear_config_after_error();
    let state = ext.state.lock().expect("lock");
    assert!(state.config.is_none());
    assert!(state.registered_agents.is_empty());
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
    let result = ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(false)));
    assert!(matches!(result, Event::ToolResult(_)));
    let state = ext.state.lock().expect("lock");
    assert!(!state.registered_agents.contains(&agent_id("agent-a")));
    assert!(state.selected_agent_by_channel.is_empty());
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

/// Registered agents send only to the configured conversation and replies are
/// prefixed with the stable agent id for Slack readers.
#[test]
fn slack_send_uses_active_conversation() {
    let (ext, _rx, client) = extension();
    register_agent(&ext, "agent-a");
    let result = ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("hello")));
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        *client.sent.lock().expect("lock"),
        vec![("C123".to_owned(), "[agent-a] hello".to_owned())]
    );
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

/// A fixed channel configuration rejects other channels and DMs instead of
/// routing prompts from an unintended Slack conversation.
#[test]
fn configured_channel_rejects_other_conversations() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C999", None, "<@UBOT123> hello"));
    assert!(rx.try_recv().is_err());
    assert_eq!(client.sent.lock().expect("lock").len(), 1);
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
            .1
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
            .1
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
            .1
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
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> select"));
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> /unknown"));
    assert!(rx.try_recv().is_err());
    assert_eq!(client.sent.lock().expect("lock").len(), 2);
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
                "ts": "1.0"
            }
        }
    })
    .to_string();
    let action = handle_socket_text(&ext, &text);
    assert_eq!(action.ack_envelope_id.as_deref(), Some("env-1"));
    ext.process_slack_message(action.message.expect("decoded message"));
    assert_eq!(recv_prompt(&rx), "[slack from U123] hello");
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
