use std::sync::Mutex;

use tau_proto::{HarnessInputMessage, ToolStarted};

use super::*;

#[derive(Default)]
struct FakeBridge {
    started: Mutex<usize>,
    registered: Mutex<HashMap<AgentId, String>>,
    sent: Mutex<Vec<(AgentId, String)>>,
}

impl FakeBridge {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl XmppBridge for FakeBridge {
    fn ensure_started(
        &self,
        _cfg: RuntimeConfig,
        _tx: mpsc::Sender<HarnessInputMessage>,
        _shutdown: Arc<AtomicBool>,
    ) -> Result<(), String> {
        *self.started.lock().expect("lock") += 1;
        Ok(())
    }

    fn register_agent(
        &self,
        cfg: &RuntimeConfig,
        session_id: &SessionId,
        agent_id: &AgentId,
    ) -> Result<String, String> {
        let address = match cfg.routing_mode {
            RoutingMode::Muc => format!(
                "{}-s{}-a{}@conference.example.org",
                cfg.muc.room_prefix,
                hex_token(session_id.as_ref()),
                hex_token(agent_id.as_ref())
            ),
            RoutingMode::DirectResource => "tau@example.org/tau-test".to_owned(),
        };
        self.registered
            .lock()
            .expect("lock")
            .insert(agent_id.clone(), address.clone());
        Ok(address)
    }

    fn unregister_agent(&self, agent_id: &AgentId) -> Result<(), String> {
        self.registered.lock().expect("lock").remove(agent_id);
        Ok(())
    }

    fn send_message(&self, agent_id: &AgentId, text: &str) -> Result<(), String> {
        self.sent
            .lock()
            .expect("lock")
            .push((agent_id.clone(), text.to_owned()));
        Ok(())
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

fn extension() -> (
    Extension,
    mpsc::Receiver<HarnessInputMessage>,
    Arc<FakeBridge>,
) {
    let (tx, rx) = mpsc::channel();
    let bridge = FakeBridge::new();
    let ext = Extension::new(bridge.clone(), tx);
    ext.apply_config(cfg());
    ext.state.lock().expect("lock").current_session_id = Some("session-1".into());
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

/// Config validation fails closed when credentials, allowlist, default
/// recipient, or MUC service information is absent or unsafe.
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

/// `xmpp_send` is gated on prior registration so an arbitrary agent cannot send
/// XMPP messages without explicitly opting into the bridge first.
#[test]
fn xmpp_send_fails_before_registration() {
    let (ext, rx, _bridge) = extension();
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("hi")));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolError(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("xmpp_register"));
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
    assert!(state.registered_agents.contains(&agent_id("agent-1")));
    assert!(state.conversations.contains_key(&agent_id("agent-1")));
}

/// MUC room identity needs the current Tau session id; registration should fail
/// before starting XMPP if the extension has not observed `session.started`.
#[test]
fn xmpp_register_requires_active_session_before_starting_bridge() {
    let (tx, rx) = mpsc::channel();
    let bridge = FakeBridge::new();
    let ext = Extension::new(bridge.clone(), tx);
    ext.apply_config(cfg());

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolError(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("active Tau session"));
    assert_eq!(*bridge.started.lock().expect("lock"), 0);
}

/// In MUC mode, two agents in the same Tau session can register at the same
/// time and receive separate stable room addresses keyed by session plus agent.
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
    assert!(agent_1.contains("s73657373696f6e2d31"));
    assert!(agent_2.contains("s73657373696f6e2d31"));
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
    let Event::ToolError(error) = *emit.event else {
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
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("hello")));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    let sent = bridge.sent.lock().expect("lock");
    assert_eq!(sent[0], (agent_id("agent-1"), "[agent-1] hello".to_owned()));
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
    let Event::ToolError(error) = *emit.event else {
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
    let mut worker = WorkerState::new(cfg(), tx);
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
    (worker, rx, room, occupant)
}

fn muc_message(from: Jid, body: &str) -> Stanza {
    let mut message =
        Message::groupchat(Jid::new("tau-agent-1@conference.example.org").expect("jid"))
            .with_body(Lang::new(), body.to_owned());
    message.from = Some(from);
    message.into()
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

/// MUC groupchat messages without real-JID proof fail closed by default so room
/// anonymity cannot silently bypass `allowed_jids`.
#[test]
fn muc_message_without_real_jid_is_not_routed() {
    let (tx, rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx);
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
    let mut message =
        Message::groupchat(Jid::new("tau-agent-1@conference.example.org").expect("jid"))
            .with_body(Lang::new(), "hello".to_owned());
    message.from = Some(Jid::new("tau-agent-1@conference.example.org/alice").expect("jid"));
    worker.handle_stanza(message.into());
    assert!(rx.try_recv().is_err());
}

/// MUC room identity is deterministic from the Tau session and agent so a
/// resumed session returns to the same XMPP conversation address.
#[test]
fn muc_room_identity_uses_stable_session_and_agent() {
    let (tx, _rx) = mpsc::channel();
    let worker = WorkerState::new(cfg(), tx);
    let room = worker
        .muc_room_for(&"session-1".into(), &agent_id("agent-1"))
        .expect("room");
    assert_eq!(
        room.to_string(),
        "tau-s73657373696f6e2d31-a6167656e742d31@conference.example.org"
    );
}

/// Full agent ids participate in MUC room identity so long ids with identical
/// display prefixes cannot collapse onto one room and overwrite inbound
/// routing.
#[test]
fn muc_room_identity_does_not_truncate_long_agent_ids() {
    let (tx, _rx) = mpsc::channel();
    let worker = WorkerState::new(cfg(), tx);
    let first = agent_id(&format!("{}{}", "a".repeat(48), "b".repeat(16)));
    let second = agent_id(&format!("{}{}", "a".repeat(48), "c".repeat(16)));

    let first_room = worker
        .muc_room_for(&"session-1".into(), &first)
        .expect("first room");
    let second_room = worker
        .muc_room_for(&"session-1".into(), &second)
        .expect("second room");

    assert_ne!(first_room, second_room);
    assert_eq!(
        first_room.to_string(),
        "tau-s73657373696f6e2d31-a61616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616162626262626262626262626262626262@conference.example.org"
    );
    assert_eq!(
        second_room.to_string(),
        "tau-s73657373696f6e2d31-a61616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616163636363636363636363636363636363@conference.example.org"
    );
}

/// Agent ids that differ only by case remain distinct after XMPP JID parsing
/// and normalization because room identity uses lowercase hex encodings, not
/// raw nodes.
#[test]
fn muc_room_identity_is_stable_across_xmpp_nodeprep_casefolding() {
    let (tx, _rx) = mpsc::channel();
    let worker = WorkerState::new(cfg(), tx);
    let uppercase = worker
        .muc_room_for(&"session-1".into(), &agent_id("AgentA"))
        .expect("uppercase room");
    let lowercase = worker
        .muc_room_for(&"session-1".into(), &agent_id("agenta"))
        .expect("lowercase room");

    assert_eq!(
        uppercase.to_string(),
        "tau-s73657373696f6e2d31-a4167656e7441@conference.example.org"
    );
    assert_eq!(
        lowercase.to_string(),
        "tau-s73657373696f6e2d31-a6167656e7461@conference.example.org"
    );
    assert_ne!(uppercase, lowercase);
}

/// Formal MUC invitations use XEP-0045 mediated invite payloads addressed to
/// the room so clients can present the room as a joinable conversation.
#[test]
fn muc_invite_message_contains_mediated_invite_payload() {
    let room = Jid::new("tau-s1-aagent-1@conference.example.org")
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

/// Allowed MUC text with a cached real JID routes exactly one prompt through
/// the harness-owned external prompt submission boundary.
#[test]
fn allowed_muc_message_routes_prompt() {
    let (tx, rx) = mpsc::channel();
    let mut worker = WorkerState::new(cfg(), tx);
    let room = Jid::new("tau-agent-1@conference.example.org")
        .expect("jid")
        .to_bare();
    let occupant = Jid::new("tau-agent-1@conference.example.org/alice").expect("jid");
    worker
        .room_to_agent
        .insert(room.clone(), agent_id("agent-1"));
    worker.occupant_real_jids.insert(
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
    let mut message =
        Message::groupchat(Jid::new("tau-agent-1@conference.example.org").expect("jid"))
            .with_body(Lang::new(), "hello".to_owned());
    message.from = Some(occupant);
    worker.handle_stanza(message.into());
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    let Event::ExtPromptSubmitRequest(req) = *emit.event else {
        panic!("prompt request")
    };
    assert_eq!(req.agent_id, agent_id("agent-1"));
    assert_eq!(
        req.text,
        "[xmpp room tau-agent-1@conference.example.org from me@example.org/dino] hello"
    );
}

/// MUC messages with an unallowlisted real JID must be dropped even when they
/// arrive in a known room.
#[test]
fn muc_message_from_unallowed_real_jid_is_not_routed() {
    let (mut worker, rx, _room, occupant) = worker_with_muc_agent();
    worker.occupant_real_jids.insert(
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
    let Event::ExtPromptSubmitRequest(req) = *emit.event else {
        panic!("prompt request")
    };
    assert_eq!(
        req.text,
        "[xmpp room tau-agent-1@conference.example.org from tau-agent-1@conference.example.org/alice] hello"
    );
}

/// The bridge must suppress groupchat echoes from its own occupant nick so
/// agent replies do not come back as fresh prompts.
#[test]
fn muc_own_message_is_not_routed() {
    let (mut worker, rx, _room, _occupant) = worker_with_muc_agent();
    let own = Jid::new("tau-agent-1@conference.example.org/tau-self").expect("jid");
    worker
        .occupant_real_jids
        .insert(own.clone(), Jid::new("me@example.org/dino").expect("jid"));
    worker.handle_stanza(muc_message(own, "echo"));
    assert!(rx.try_recv().is_err());
}

/// Oversized inbound MUC text is dropped before prompt submission to bound
/// external prompt amplification.
#[test]
fn oversized_muc_message_is_not_routed() {
    let (mut worker, rx, _room, occupant) = worker_with_muc_agent();
    worker.cfg.max_message_bytes = 4;
    worker.occupant_real_jids.insert(
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
    worker.occupant_real_jids.insert(
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
    worker
        .occupant_real_jids
        .insert(occupant, Jid::new("me@example.org/dino").expect("jid"));
    worker.apply_online_state(Jid::new("tau@example.org/new").expect("jid"));
    assert!(worker.occupant_real_jids.is_empty());
}

/// Direct-resource reconnect handling updates the stored full JID and returns a
/// notification work item so the human recipient can learn the new address.
#[test]
fn online_state_updates_direct_resource_full_jid() {
    let (tx, _rx) = mpsc::channel();
    let mut cfg = cfg();
    cfg.routing_mode = RoutingMode::DirectResource;
    let mut worker = WorkerState::new(cfg, tx);
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
    worker.occupant_real_jids.insert(
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
    let mut worker = WorkerState::new(cfg, tx);
    let bound = Jid::new("tau@example.org/tau-resource").expect("jid");
    worker.bound_jid = Some(bound.clone());
    worker.conversations.insert(
        agent_id("agent-1"),
        Conversation::Direct {
            full_jid: bound.clone(),
        },
    );

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
    let Event::ExtPromptSubmitRequest(req) = *emit.event else {
        panic!("prompt request")
    };
    assert_eq!(req.text, "[xmpp direct from me@example.org/dino] hello");
}

/// Direct-resource fallback refuses a second registration because one bound JID
/// cannot provide unambiguous one-to-one inbound routing for multiple agents.
#[tokio::test]
async fn direct_registration_rejects_second_agent() {
    let (tx, _rx) = mpsc::channel();
    let mut cfg = cfg();
    cfg.routing_mode = RoutingMode::DirectResource;
    let mut worker = WorkerState::new(cfg, tx);
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
        .register_agent("session-1".into(), agent_id("agent-2"), &mut client)
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
    let mut worker = WorkerState::new(cfg(), tx);
    let agent = agent_id("agent-1");
    let room = Jid::new("tau-agent-1@conference.example.org")
        .expect("jid")
        .to_bare();
    worker.room_to_agent.insert(room.clone(), agent.clone());
    worker.conversations.insert(
        agent.clone(),
        Conversation::Muc {
            room,
            nick: "tau-self".to_owned(),
        },
    );

    let removed = worker
        .remove_conversation(&agent)
        .expect("removed conversation");

    assert!(!worker.conversations.contains_key(&agent));
    assert!(worker.room_to_agent.values().all(|mapped| mapped != &agent));
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
