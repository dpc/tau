use std::sync::Mutex;

use tau_proto::{HarnessInputMessage, ToolStarted};

use super::*;

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

/// Telegram bridge tools are disabled by default because each role must make an
/// explicit policy choice before exposing the external chat bridge to a model.
#[test]
fn telegram_tools_are_role_opt_in() {
    assert!(!register_tool_spec().enabled_by_default);
    assert!(!send_tool_spec().enabled_by_default);
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
    let state = ext.state.lock().expect("lock");
    assert!(state.registered_agents.contains(&agent_id("agent-1")));
    assert!(state.poller_started);
}

/// Messages from users outside the allowlist must not become Tau prompts.
#[test]
fn incoming_unallowed_user_is_not_routed() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .expect("lock")
        .registered_agents
        .insert(agent_id("agent-1"));
    ext.process_update(TgUpdate {
        update_id: 1,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: None,
            user_id: 999,
            from_name: None,
            text: Some("hello".to_owned()),
        }),
    });
    assert!(rx.try_recv().is_err());
}

/// With exactly one registered agent, plain Telegram text is submitted through
/// the harness-owned prompt request path with a source prefix.
#[test]
fn one_registered_agent_routes_plain_text() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .expect("lock")
        .registered_agents
        .insert(agent_id("agent-1"));
    ext.process_update(TgUpdate {
        update_id: 1,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: None,
            user_id: 123,
            from_name: Some("alice".to_owned()),
            text: Some("hello".to_owned()),
        }),
    });
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
        let mut state = ext.state.lock().expect("lock");
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
    }
    ext.process_update(TgUpdate {
        update_id: 1,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: None,
            user_id: 123,
            from_name: None,
            text: Some("hello".to_owned()),
        }),
    });
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
        let mut state = ext.state.lock().expect("lock");
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
        state
            .agent_labels
            .insert(agent_id("agent-1"), "Alpha".to_owned());
        state
            .agent_labels
            .insert(agent_id("agent-2"), "Beta".to_owned());
    }

    ext.process_update(TgUpdate {
        update_id: 1,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: None,
            user_id: 123,
            from_name: None,
            text: Some("/agents".to_owned()),
        }),
    });
    ext.process_update(TgUpdate {
        update_id: 2,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: None,
            user_id: 123,
            from_name: None,
            text: Some("/select agent-2".to_owned()),
        }),
    });

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
        let mut state = ext.state.lock().expect("lock");
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

    ext.process_update(TgUpdate {
        update_id: 1,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: None,
            user_id: 123,
            from_name: None,
            text: Some("/agents".to_owned()),
        }),
    });

    assert_eq!(
        client.sent.lock().expect("lock")[0].1,
        "Registered Tau agents:\n- agent-1\n- agent-2\n- agent-3"
    );
}

/// `/select` stores a chat-local target so later plain text can be routed even
/// while multiple agents are registered.
#[test]
fn select_then_plain_text_routes_to_selected_agent() {
    let (ext, rx, _client) = extension();
    {
        let mut state = ext.state.lock().expect("lock");
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
    }
    ext.process_update(TgUpdate {
        update_id: 1,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: None,
            user_id: 123,
            from_name: None,
            text: Some("/select agent-2".to_owned()),
        }),
    });
    ext.process_update(TgUpdate {
        update_id: 2,
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: None,
            user_id: 123,
            from_name: None,
            text: Some("hi".to_owned()),
        }),
    });
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    let Event::ExtPromptSubmitRequest(req) = *emit.event else {
        panic!("prompt request")
    };
    assert_eq!(req.agent_id, agent_id("agent-2"));
}

/// The model can pass only `message`; even if extra arguments appear, outgoing
/// delivery must use the configured/linked chat id and an agent-id-only prefix.
#[test]
fn telegram_send_uses_configured_chat_not_argument_chat_id() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock().expect("lock");
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
    let _result = rx.recv().expect("result");
    let sent = client.sent.lock().expect("lock");
    assert_eq!(sent[0].0, 123);
    assert_eq!(sent[0].1, "[agent-1] hello");
}

/// Group chats are refused unless the user explicitly configured that chat id;
/// this keeps the MVP private-chat oriented by default.
#[test]
fn unconfigured_group_chat_is_refused() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock().expect("lock");
        state.config.as_mut().expect("config").configured_chat_id = None;
        state.learned_chat_id = None;
        state.registered_agents.insert(agent_id("agent-1"));
    }
    ext.process_update(TgUpdate {
        update_id: 1,
        message: Some(TgMessage {
            chat_id: -100,
            chat_type: Some("supergroup".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("hello".to_owned()),
        }),
    });
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
        let mut state = ext.state.lock().expect("lock");
        state.config.as_mut().expect("config").configured_chat_id = Some(-100);
        state.registered_agents.insert(agent_id("agent-1"));
    }
    ext.process_update(TgUpdate {
        update_id: 1,
        message: Some(TgMessage {
            chat_id: -100,
            chat_type: Some("supergroup".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("hello".to_owned()),
        }),
    });
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    assert!(matches!(*emit.event, Event::ExtPromptSubmitRequest(_)));
}

/// Allowlist checks run before group handling, so an unallowed group user
/// cannot trigger either a Tau prompt or a Telegram reply from the bridge.
#[test]
fn unallowed_group_user_cannot_route() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .expect("lock")
        .registered_agents
        .insert(agent_id("agent-1"));
    ext.process_update(TgUpdate {
        update_id: 1,
        message: Some(TgMessage {
            chat_id: -100,
            chat_type: Some("supergroup".to_owned()),
            user_id: 999,
            from_name: None,
            text: Some("hello".to_owned()),
        }),
    });
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
