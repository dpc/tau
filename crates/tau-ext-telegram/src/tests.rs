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
}

impl FakeClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            update_batches: Mutex::new(Vec::new()),
            poll_timeouts: Mutex::new(Vec::new()),
        })
    }

    fn with_updates(update_batches: Vec<Vec<TgUpdate>>) -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            update_batches: Mutex::new(update_batches),
            poll_timeouts: Mutex::new(Vec::new()),
        })
    }
}

impl TelegramClient for FakeClient {
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
        self.sent
            .lock()
            .expect("lock")
            .push((chat_id, text.to_owned()));
        Ok(())
    }
}

struct SlowPollClient;

impl TelegramClient for SlowPollClient {
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
    ext.apply_config(cfg(), None);
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
        .err()
        .expect("missing token secret");
    assert!(err.contains("bot_token_secret"));

    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    let err = ExtConfig {
        bot_token_secret: Some("bot".to_owned()),
        ..Default::default()
    }
    .validate(&secrets)
    .err()
    .expect("empty allowlist");
    assert!(err.contains("allowed_user_ids"));
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
        .err()
        .expect("unsafe api_base should be rejected");
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

/// With exactly one registered agent, plain Telegram text is submitted through
/// the harness-owned prompt request path with a source prefix.
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
    let Event::ExtPromptSubmitRequest(req) = *emit.event else {
        panic!("prompt request")
    };
    assert_eq!(req.agent_id, agent_id("agent-1"));
    assert_eq!(req.text, "[telegram from alice] hello");
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
    let Event::ExtPromptSubmitRequest(req) = *emit.event else {
        panic!("prompt request")
    };
    assert_eq!(req.agent_id, agent_id("agent-2"));
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
    assert!(matches!(*emit.event, Event::ExtPromptSubmitRequest(_)));
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
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");

    let sent = client.sent.lock().expect("lock");
    assert_eq!(sent[0].0, 123);
    assert_eq!(sent[1].0, 456);
    assert_eq!(sent[2], (123, "[agent-1] reply".to_owned()));
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
    ext.apply_config(new_cfg, None);
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
    ext.apply_config(cfg(), None);
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
    ext.apply_config(cfg(), None);
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("fresh prompt") else {
        panic!("emit")
    };
    let Event::ExtPromptSubmitRequest(req) = *emit.event else {
        panic!("prompt request")
    };
    assert_eq!(req.text, "[telegram from alice] fresh");
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
            instance_name: None,
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: None,
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
            instance_name: None,
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: None,
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
            instance_name: None,
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: None,
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
            instance_name: None,
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: None,
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
            instance_name: None,
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "unknown_field": true,
            })),
            state_dir: None,
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
    ext.apply_config(cfg(), None);
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("fresh prompt") else {
        panic!("emit")
    };
    let Event::ExtPromptSubmitRequest(req) = *emit.event else {
        panic!("prompt request")
    };
    assert_eq!(req.text, "[telegram from alice] fresh");
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
    ext.apply_config(new_cfg, None);

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
    ext.apply_config(new_cfg, None);

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
    ext.apply_config(new_cfg, None);

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
    ext.apply_config(cfg(), None);
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    client.wait_for_call();

    let mut new_cfg = cfg();
    new_cfg.bot_token = "different-token".to_owned();
    ext.apply_config(new_cfg, None);
    client.release_first_response(Vec::new());
    std::thread::sleep(Duration::from_millis(100));

    let state = ext.state.lock();
    assert!(!state.poller_drained_initial_backlog);
    assert_eq!(state.next_update_offset, None);
    assert!(rx.try_recv().is_err());
}

/// Non-empty poll responses from an old config generation must also be
/// discarded, avoiding both stale offset updates and prompt submission under
/// the new config.
#[test]
fn old_generation_non_empty_poll_response_does_not_route_or_advance_offset() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg(), None);
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    client.wait_for_call();

    let mut new_cfg = cfg();
    new_cfg.api_base = "http://127.0.0.1:1234".to_owned();
    ext.apply_config(new_cfg, None);
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
    ext.apply_config(cfg(), None);
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
    let Event::ExtPromptSubmitRequest(req) = *emit.event else {
        panic!("prompt request")
    };
    assert_eq!(req.text, "[telegram from alice] fresh after reregister");
}

/// Error backoff uses the shared-state condvar so a config change wakes it
/// promptly instead of waiting for the full local retry delay.
#[test]
fn poll_error_backoff_wakes_on_config_change() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg(), None);
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);

    client.wait_for_call_count(1);
    client.release_first_response(Vec::new());
    client.wait_for_call_count(2);
    client.release_error("temporary failure");

    let mut new_cfg = cfg();
    new_cfg.poll_timeout_seconds = 2;
    ext.apply_config(new_cfg, None);
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
    ext.apply_config(cfg(), None);
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
    ext.apply_config(new_cfg, None);
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
