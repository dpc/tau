use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex, mpsc};

use super::*;
use crate::api::{RejectedOperation, SentMessage, ZulipClient};

/// Cloneable in-memory sink used to inspect tracing output.
#[derive(Clone, Default)]
struct SharedTraceWriter {
    /// Bytes written by the temporary tracing subscriber.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedTraceWriter {
    /// Return all tracing bytes captured so far.
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("trace writer lock").clone()
    }
}

impl std::io::Write for SharedTraceWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes
            .lock()
            .expect("trace writer lock")
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Deterministic API fake retaining calls without network access.
#[derive(Default)]
struct FakeClient {
    /// Next queue registration outcome.
    register_error: Mutex<Option<ApiError>>,
    /// Sent route and content observations.
    sends: Mutex<Vec<(NativeRoute, String)>>,
    /// Reaction observations.
    reactions: Mutex<Vec<(u64, String, bool)>>,
    /// Optional signal emitted when queue registration starts.
    register_started: Mutex<Option<mpsc::Sender<()>>>,
    /// Optional gate delaying queue registration completion.
    register_release: Mutex<Option<mpsc::Receiver<()>>>,
}

impl ZulipClient for FakeClient {
    fn register_queue(&self, _cfg: &RuntimeConfig) -> Result<EventQueue, ApiError> {
        if let Some(started) = self
            .register_started
            .lock()
            .expect("register started lock")
            .take()
        {
            started.send(()).expect("signal register start");
        }
        if let Some(release) = self
            .register_release
            .lock()
            .expect("register release lock")
            .take()
        {
            release.recv().expect("release register");
        }
        if let Some(error) = self.register_error.lock().expect("register lock").take() {
            return Err(error);
        }
        Ok(EventQueue {
            queue_id: "queue-secret".to_owned(),
            last_event_id: 10,
            bot_user_id: 99,
            bot_full_name: Some("Tau Bot".to_owned()),
            poll_request_timeout: Duration::from_secs(100),
        })
    }

    fn get_events(
        &self,
        _cfg: &RuntimeConfig,
        _queue_id: &str,
        _last_event_id: i64,
        _request_timeout: Duration,
    ) -> Result<Vec<serde_json::Value>, ApiError> {
        Err(ApiError::unavailable())
    }

    fn send_message(
        &self,
        _cfg: &RuntimeConfig,
        route: &NativeRoute,
        content: &str,
    ) -> Result<SentMessage, ApiError> {
        self.sends
            .lock()
            .expect("sends lock")
            .push((route.clone(), content.to_owned()));
        Ok(SentMessage { message_id: 777 })
    }

    fn react(
        &self,
        _cfg: &RuntimeConfig,
        message_id: u64,
        emoji_name: &str,
        add: bool,
    ) -> Result<(), ApiError> {
        self.reactions.lock().expect("reactions lock").push((
            message_id,
            emoji_name.to_owned(),
            add,
        ));
        Ok(())
    }
}

fn cfg() -> RuntimeConfig {
    RuntimeConfig {
        email: "bot@example.test".to_owned(),
        api_key: "top-secret".to_owned(),
        api_base: "http://127.0.0.1:1/api/v1".to_owned(),
        allowed_user_ids: HashSet::from([42]),
        sender_aliases: HashMap::from([(42, "alice".to_owned())]),
        routes: vec![StreamRoute {
            alias: "ops".to_owned(),
            stream_id: 7,
            topic: Some("deploy".to_owned()),
            receive: Some(ReceiveMode::MentionsOnly),
            proactive: ProactiveRoute::ExactTopic("deploy".to_owned()),
            description: Some("Operations".to_owned()),
        }],
        receive_direct_messages: true,
        max_message_bytes: 1024,
        id_key: [7; 32],
    }
}

/// Validate a minimal stream-route configuration with deterministic test
/// secrets.
fn validated_config(conversations: serde_json::Value) -> Result<RuntimeConfig, String> {
    let config = CborValue::serialized(&serde_json::json!({
        "bot_email_secret":"email",
        "api_key_secret":"key",
        "identity_key_secret":"identity",
        "site":"https://chat.example.test",
        "allowed_user_ids":[42],
        "conversations":conversations,
    }))
    .expect("config value")
    .deserialized::<ExtConfig>()
    .expect("config schema");
    let secrets = BTreeMap::from([
        (
            "email".to_owned(),
            tau_proto::SecretValue::new("bot@example.test"),
        ),
        ("key".to_owned(), tau_proto::SecretValue::new("secret")),
        (
            "identity".to_owned(),
            tau_proto::SecretValue::new("stable-pseudonym-key"),
        ),
    ]);
    config.validate(&secrets)
}

fn agent_id() -> AgentId {
    AgentId::parse("agent").expect("agent id")
}
fn publisher() -> tau_proto::ExtensionName {
    tau_proto::ExtensionName::parse("std-zulip").expect("publisher")
}
fn extension() -> (
    Extension,
    mpsc::Receiver<HarnessInputMessage>,
    Arc<FakeClient>,
) {
    extension_with_config(cfg())
}

/// Build a configured extension with deterministic local I/O.
fn extension_with_config(
    config: RuntimeConfig,
) -> (
    Extension,
    mpsc::Receiver<HarnessInputMessage>,
    Arc<FakeClient>,
) {
    let (tx, rx) = mpsc::channel();
    let client = Arc::new(FakeClient::default());
    let ext = Extension::new(client.clone(), tx, ToolNames::logical());
    ext.apply_config(config, publisher());
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id());
        state.queue = Some(EventQueue {
            queue_id: "queue-secret".to_owned(),
            last_event_id: 0,
            bot_user_id: 99,
            bot_full_name: Some("Tau Bot".to_owned()),
            poll_request_timeout: Duration::from_secs(100),
        });
        state.registration_generation = 1;
    }
    (ext, rx, client)
}

fn tool(name: &str, fields: Vec<(&str, CborValue)>) -> ToolStarted {
    ToolStarted {
        call_id: format!("call-{name}").into(),
        tool_name: tau_proto::ToolName::new(name),
        arguments: CborValue::Map(
            fields
                .into_iter()
                .map(|(name, value)| (CborValue::Text(name.to_owned()), value))
                .collect(),
        ),
        agent_id: agent_id(),
        originator: tau_proto::PromptOriginator::User,
    }
}

fn event_from(message: HarnessInputMessage) -> Event {
    let HarnessInputMessage::Emit(emit) = message else {
        panic!("expected emit")
    };
    *emit.event
}

/// Configuration rejects unknown fields and an empty mandatory sender allowlist
/// before network startup.
#[test]
fn configuration_validation_is_strict() {
    let value = CborValue::serialized(&serde_json::json!({
        "bot_email_secret":"email", "api_key_secret":"key", "site":"http://example.com",
        "allowed_user_ids":[], "unknown":true
    }))
    .expect("config value");
    assert!(value.deserialized::<ExtConfig>().is_err());
    let raw = ExtConfig {
        bot_email_secret: Some("email".to_owned()),
        api_key_secret: Some("key".to_owned()),
        identity_key_secret: Some("identity".to_owned()),
        ..Default::default()
    };
    let secrets = BTreeMap::from([
        (
            "email".to_owned(),
            tau_proto::SecretValue::new("bot@example.test"),
        ),
        ("key".to_owned(), tau_proto::SecretValue::new("secret")),
        (
            "identity".to_owned(),
            tau_proto::SecretValue::new("stable-pseudonym-key"),
        ),
    ]);
    let Err(error) = raw.validate(&secrets) else {
        panic!("empty allowlist accepted")
    };
    assert_eq!(error, "zulip config requires non-empty `allowed_user_ids`");
}

/// Channel-wide topic authority requires both proactive-send authority and no
/// configured exact topic, preventing a typo from silently widening an alias.
#[test]
fn agent_chosen_topic_configuration_fails_closed() {
    let secrets = BTreeMap::from([
        (
            "email".to_owned(),
            tau_proto::SecretValue::new("bot@example.test"),
        ),
        ("key".to_owned(), tau_proto::SecretValue::new("secret")),
        (
            "identity".to_owned(),
            tau_proto::SecretValue::new("stable-pseudonym-key"),
        ),
    ]);
    let parse = |conversation: serde_json::Value| {
        CborValue::serialized(&serde_json::json!({
            "bot_email_secret":"email",
            "api_key_secret":"key",
            "identity_key_secret":"identity",
            "site":"https://chat.example.test",
            "allowed_user_ids":[42],
            "conversations":[conversation],
        }))
        .expect("config value")
        .deserialized::<ExtConfig>()
        .expect("config schema")
    };
    let Err(error) = parse(serde_json::json!({
        "alias":"channel", "stream_id":7, "topic":"deploy",
        "proactive_send":true, "agent_chosen_topic":true
    }))
    .validate(&secrets) else {
        panic!("exact route accepted agent-chosen topic authority")
    };
    assert_eq!(
        error,
        "zulip `agent_chosen_topic` routes must omit the configured `topic`"
    );
    let Err(error) = parse(serde_json::json!({
        "alias":"channel", "stream_id":7, "receive":"mentions_only",
        "agent_chosen_topic":true
    }))
    .validate(&secrets) else {
        panic!("non-proactive route accepted agent-chosen topic authority")
    };
    assert_eq!(
        error,
        "zulip `agent_chosen_topic` requires `proactive_send: true`"
    );
    let config = parse(serde_json::json!({
        "alias":"channel", "stream_id":7,
        "proactive_send":true, "agent_chosen_topic":true
    }))
    .validate(&secrets)
    .expect("valid channel-wide authority");
    assert!(config.routes[0].proactive.allows_agent_chosen_topic());
    assert!(config.routes[0].topic.is_none());
}

/// A topicless agent-chosen route may receive every topic in its stream, while
/// receive overlap remains rejected and a send-only route can share that
/// stream.
#[test]
fn agent_chosen_topic_receive_and_collision_rules_are_explicit() {
    let all_topics = validated_config(serde_json::json!([{
        "alias":"channel", "stream_id":7, "receive":"mentions_only",
        "proactive_send":true, "agent_chosen_topic":true
    }]))
    .expect("valid all-topic receive route");
    let (ext, rx, _) = extension_with_config(all_topics);
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    ext.process_event(
        serde_json::json!({
            "id":16, "type":"message", "flags":["mentioned"], "message": {
                "id":505, "type":"stream", "sender_id":42, "stream_id":7,
                "subject":"any topic", "content":"@**Tau Bot** all topics"
            }
        }),
        generation,
        registration,
        99,
        Some("Tau Bot"),
    );
    assert!(matches!(
        event_from(rx.recv().expect("all-topic receive report")),
        Event::MessageDeliveredReported(_)
    ));

    let send_and_exact_receive = validated_config(serde_json::json!([
        {
            "alias":"channel", "stream_id":7,
            "proactive_send":true, "agent_chosen_topic":true
        },
        {
            "alias":"deploy", "stream_id":7, "topic":"deploy",
            "receive":"mentions_only"
        }
    ]));
    assert!(send_and_exact_receive.is_ok());

    let Err(error) = validated_config(serde_json::json!([
        {
            "alias":"channel", "stream_id":7, "receive":"mentions_only",
            "proactive_send":true, "agent_chosen_topic":true
        },
        {
            "alias":"deploy", "stream_id":7, "topic":"deploy",
            "receive":"mentions_only"
        }
    ])) else {
        panic!("overlapping receive routes accepted")
    };
    assert_eq!(
        error,
        "a receive-all-topics Zulip route cannot overlap another receive route"
    );
}

/// Pseudonymization identity remains stable across API-key rotation and changes
/// only when the explicitly stable identity secret rotates.
#[test]
fn identity_key_is_independent_of_api_credentials() {
    fn validate(api_key: &str, identity_key: &str) -> RuntimeConfig {
        let raw = ExtConfig {
            bot_email_secret: Some("email".to_owned()),
            api_key_secret: Some("key".to_owned()),
            identity_key_secret: Some("identity".to_owned()),
            site: Some("https://chat.example.test".to_owned()),
            allowed_user_ids: vec![42],
            ..Default::default()
        };
        let secrets = BTreeMap::from([
            (
                "email".to_owned(),
                tau_proto::SecretValue::new("bot@example.test"),
            ),
            ("key".to_owned(), tau_proto::SecretValue::new(api_key)),
            (
                "identity".to_owned(),
                tau_proto::SecretValue::new(identity_key),
            ),
        ]);
        raw.validate(&secrets).expect("valid config")
    }

    let original = validate("api-key-one", "stable-identity-key");
    let api_rotated = validate("api-key-two", "stable-identity-key");
    let identity_rotated = validate("api-key-two", "new-stable-identity-key");
    assert_eq!(original.id_key, api_rotated.id_key);
    assert_ne!(original.id_key, identity_rotated.id_key);
}

/// Mention-only stream ingress admits an exact allowlisted sender, strips one
/// leading transport mention, and emits no native authority.
#[test]
fn stream_ingress_emits_bounded_report() {
    let (ext, rx, _) = extension();
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    ext.process_event(
        serde_json::json!({
            "id":11, "type":"message", "flags":["mentioned"], "message": {
                "id":500, "type":"stream", "sender_id":42, "stream_id":7,
                "subject":"deploy", "content":"@**Tau Bot** restart service"
            }
        }),
        generation,
        registration,
        99,
        Some("Tau Bot"),
    );
    let Event::MessageDeliveredReported(report) = event_from(rx.recv().expect("delivery report"))
    else {
        panic!("wrong report")
    };
    assert_eq!(report.text, "restart service");
    assert_eq!(report.sender.display_name.as_deref(), Some("alice"));
    assert_eq!(
        report
            .conversation
            .as_ref()
            .expect("conversation")
            .alias
            .as_deref(),
        Some("ops")
    );
    let wire = serde_json::to_string(&report).expect("report json");
    assert!(!wire.contains("stream_id"));
    assert!(!wire.contains("stream:7"));
    assert!(!wire.contains("topic:deploy"));
    assert!(!wire.contains("\"42\""));
    assert!(!wire.contains("\"500\""));
}

/// Direct-message identity derives from sorted participant IDs and source
/// replies retain the exact frozen participant set.
#[test]
fn direct_message_reply_is_source_bound() {
    let (ext, _rx, client) = extension();
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    ext.process_event(
        serde_json::json!({
            "id":12, "type":"message", "message": {
                "id":501, "type":"private", "sender_id":42, "content":"hello", "flags":[],
                "display_recipient":[{"id":99},{"id":42}]
            }
        }),
        generation,
        registration,
        99,
        Some("Tau Bot"),
    );
    let reference = message_fact_id(&cfg(), 501).as_str().to_owned();
    let result = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("reply".to_owned())),
            ("reply_to", CborValue::Text(reference)),
        ],
    ));
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        client.sends.lock().expect("sends").as_slice(),
        &[(NativeRoute::Direct(vec![42]), "reply".to_owned())]
    );
}

/// Successful send writes `message.sent_reported` before its terminal result
/// and returns only an opaque Tau reference.
#[test]
fn send_report_precedes_tool_result() {
    let (ext, rx, _) = extension();
    ext.dispatch_tool(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("deploy now".to_owned())),
            ("destination", CborValue::Text("ops".to_owned())),
        ],
    ));
    assert!(matches!(
        event_from(rx.recv().expect("progress")),
        Event::ToolProgressReported(_)
    ));
    assert!(matches!(
        event_from(rx.recv().expect("sent report")),
        Event::MessageSentReported(_)
    ));
    let Event::ToolResultReported(result) = event_from(rx.recv().expect("result")) else {
        panic!("missing result")
    };
    let CborValue::Text(text) = result.result else {
        panic!("text result")
    };
    assert!(text.contains("zulip-message:"));
    assert!(!text.contains("777"));
}

/// An explicit channel-wide grant lets an agent choose a bounded topic,
/// including Zulip's canonical empty topic for general chat, without exposing a
/// stream ID, and keeps the documented 256-byte maximum from regressing.
#[test]
fn agent_chosen_topic_destination_sends_to_general_chat() {
    let mut config = cfg();
    let route = config.routes.first_mut().expect("configured route");
    route.topic = None;
    route.proactive = ProactiveRoute::AgentChosenTopic;
    let (ext, _rx, client) = extension_with_config(config);
    let result = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("hello general chat".to_owned())),
            ("destination", CborValue::Text("ops".to_owned())),
            ("topic", CborValue::Text(String::new())),
        ],
    ));
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        client.sends.lock().expect("sends").as_slice(),
        &[(
            NativeRoute::Stream {
                stream_id: 7,
                topic: String::new(),
            },
            "hello general chat".to_owned(),
        )]
    );
    let maximum_topic = "x".repeat(256);
    let maximum = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("maximum topic".to_owned())),
            ("destination", CborValue::Text("ops".to_owned())),
            ("topic", CborValue::Text(maximum_topic.clone())),
        ],
    ));
    assert!(matches!(maximum, Event::ToolResult(_)));
    assert_eq!(
        client.sends.lock().expect("sends").last(),
        Some(&(
            NativeRoute::Stream {
                stream_id: 7,
                topic: maximum_topic,
            },
            "maximum topic".to_owned(),
        ))
    );
    let missing_topic = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("missing topic".to_owned())),
            ("destination", CborValue::Text("ops".to_owned())),
        ],
    ));
    assert!(matches!(missing_topic, Event::ToolError(_)));
    let whitespace_topic = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("whitespace topic".to_owned())),
            ("destination", CborValue::Text("ops".to_owned())),
            ("topic", CborValue::Text(" ".to_owned())),
        ],
    ));
    assert!(matches!(whitespace_topic, Event::ToolError(_)));
    let oversized_topic = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("oversized topic".to_owned())),
            ("destination", CborValue::Text("ops".to_owned())),
            ("topic", CborValue::Text("x".repeat(257))),
        ],
    ));
    assert!(matches!(oversized_topic, Event::ToolError(_)));
    let control_topic = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("control topic".to_owned())),
            ("destination", CborValue::Text("ops".to_owned())),
            ("topic", CborValue::Text("\u{1b}".to_owned())),
        ],
    ));
    assert!(matches!(control_topic, Event::ToolError(_)));
    assert_eq!(client.sends.lock().expect("sends").len(), 2);
}

/// The send schema exposes `topic` as an optional string and wrong-typed
/// selectors fail before any remote send can widen or choose a destination.
#[test]
fn send_schema_and_selector_types_fail_closed() {
    let spec = send_spec(&ToolNames::logical());
    let parameters = spec.parameters.expect("send parameters");
    assert_eq!(
        parameters.pointer("/properties/topic/type"),
        Some(&serde_json::Value::String("string".to_owned()))
    );
    assert_eq!(
        parameters.get("additionalProperties"),
        Some(&serde_json::Value::Bool(false))
    );

    let (ext, _rx, client) = extension();
    for fields in [
        vec![
            ("message", CborValue::Text("bad topic".to_owned())),
            ("destination", CborValue::Text("ops".to_owned())),
            ("topic", CborValue::Bool(true)),
        ],
        vec![
            ("message", CborValue::Text("bad destination".to_owned())),
            ("destination", CborValue::Bool(true)),
        ],
        vec![
            ("message", CborValue::Text("bad reply".to_owned())),
            ("reply_to", CborValue::Bool(true)),
        ],
    ] {
        assert!(matches!(
            ext.handle_send(tool(SEND_TOOL_NAME, fields)),
            Event::ToolError(_)
        ));
    }
    assert!(client.sends.lock().expect("sends").is_empty());
}

/// Exact aliases and source-bound replies reject caller-selected topics so
/// their configured and admitted source routes cannot be widened by a tool
/// call.
#[test]
fn exact_and_reply_sends_reject_caller_selected_topics() {
    let (ext, _rx, client) = extension();
    let exact = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("wrong route".to_owned())),
            ("destination", CborValue::Text("ops".to_owned())),
            ("topic", CborValue::Text("other".to_owned())),
        ],
    ));
    assert!(matches!(exact, Event::ToolError(_)));

    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    ext.process_event(
        serde_json::json!({
            "id":15, "type":"message", "message": {
                "id":504, "type":"private", "sender_id":42, "content":"hello", "flags":[],
                "display_recipient":[{"id":99},{"id":42}]
            }
        }),
        generation,
        registration,
        99,
        Some("Tau Bot"),
    );
    let reply = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("wrong reply route".to_owned())),
            (
                "reply_to",
                CborValue::Text(message_fact_id(&cfg(), 504).as_str().to_owned()),
            ),
            ("topic", CborValue::Text("other".to_owned())),
        ],
    ));
    assert!(matches!(reply, Event::ToolError(_)));
    assert!(client.sends.lock().expect("sends").is_empty());
}

/// Discovery makes the exceptional channel-wide topic authority explicit while
/// leaving the configured stream identifier extension-private.
#[test]
fn discovery_marks_agent_chosen_topic_authority() {
    let mut config = cfg();
    let route = config.routes.first_mut().expect("configured route");
    route.topic = None;
    route.proactive = ProactiveRoute::AgentChosenTopic;
    let (ext, _rx, _client) = extension_with_config(config);
    let Event::ToolResult(result) = ext.handle_conversations(tool(CONVERSATIONS_TOOL_NAME, vec![]))
    else {
        panic!("expected discovery result");
    };
    let CborValue::Text(value) = result.result else {
        panic!("expected JSON discovery result");
    };
    let value: serde_json::Value = serde_json::from_str(&value).expect("valid discovery JSON");
    assert_eq!(
        value.pointer("/conversations/0/kind"),
        Some(&serde_json::Value::String("stream".to_owned()))
    );
    assert_eq!(
        value.pointer("/conversations/0/agent_chosen_topic"),
        Some(&serde_json::Value::Bool(true))
    );
    assert!(!value.to_string().contains("stream_id"));
}

/// Duplicate native creates and self-authored messages are suppressed before
/// report emission.
#[test]
fn duplicate_and_self_messages_are_suppressed() {
    let (ext, rx, _) = extension();
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    let event = serde_json::json!({"id":13,"type":"message","message":{"id":502,"type":"private","sender_id":42,"content":"once","flags":[],"display_recipient":[{"id":42},{"id":99}]}});
    ext.process_event(event.clone(), generation, registration, 99, Some("Tau Bot"));
    ext.process_event(event, generation, registration, 99, Some("Tau Bot"));
    ext.process_event(serde_json::json!({"id":14,"type":"message","message":{"id":503,"type":"private","sender_id":99,"content":"echo","flags":[]}}), generation, registration, 99, Some("Tau Bot"));
    assert!(matches!(
        event_from(rx.recv().expect("one report")),
        Event::MessageDeliveredReported(_)
    ));
    assert!(rx.try_recv().is_err());
}

/// A stale registration generation cannot submit an event after unregister or
/// retarget authority changes.
#[test]
fn stale_generation_drops_ingress() {
    let (ext, rx, _) = extension();
    let generation = ext.state.lock().config_generation;
    ext.state.lock().registration_generation = 2;
    ext.process_event(serde_json::json!({"id":15,"type":"message","message":{"id":504,"type":"private","sender_id":42,"content":"stale","flags":[]}}), generation, 1, 99, Some("Tau Bot"));
    assert!(rx.try_recv().is_err());
}

/// A later unregister intent synchronously supersedes an earlier queue
/// registration even when the earlier network request completes last.
#[test]
fn delayed_enable_cannot_defeat_later_unregister() {
    let (tx, rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let client = Arc::new(FakeClient::default());
    *client.register_started.lock().expect("started lock") = Some(started_tx);
    *client.register_release.lock().expect("release lock") = Some(release_rx);
    let ext = Extension::new(client, tx, ToolNames::logical());
    ext.apply_config(cfg(), publisher());

    let mut enable = tool(REGISTER_TOOL_NAME, vec![("enabled", CborValue::Bool(true))]);
    enable.call_id = "enable-call".into();
    ext.dispatch_tool(enable);
    assert!(matches!(
        event_from(rx.recv().expect("enable progress")),
        Event::ToolProgressReported(_)
    ));
    started_rx.recv().expect("registration started");

    let mut disable = tool(
        REGISTER_TOOL_NAME,
        vec![("enabled", CborValue::Bool(false))],
    );
    disable.call_id = "disable-call".into();
    ext.dispatch_tool(disable);
    assert!(matches!(
        event_from(rx.recv().expect("disable progress")),
        Event::ToolProgressReported(_)
    ));
    assert!(matches!(
        event_from(rx.recv().expect("disable result")),
        Event::ToolResultReported(_)
    ));
    release_tx.send(()).expect("release enable");
    assert!(matches!(
        event_from(rx.recv().expect("superseded enable")),
        Event::ToolErrorReported(_)
    ));
    assert!(!ext.state.lock().registered_agents.contains(&agent_id()));
}

/// Queue expiry emits a content-free gap notice and replaces the opaque queue
/// without exposing either queue identifier.
#[test]
fn queue_expiry_reports_gap_and_registers_fresh_queue() {
    let (ext, rx, _) = extension();
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    let config = state.config.clone().expect("config");
    drop(state);
    handle_queue_expiry(&ext, &config, generation, registration);
    let HarnessInputMessage::ExtensionNoticeRequest(notice) = rx.recv().expect("gap notice") else {
        panic!("wrong notice");
    };
    assert!(notice.message.contains("may have been missed"));
    assert!(!notice.message.contains("queue-secret"));
    assert_eq!(
        ext.state
            .lock()
            .queue
            .as_ref()
            .expect("fresh queue")
            .last_event_id,
        10
    );
}

/// Failed live queue re-registration logs the bounded category field and omits
/// untrusted malformed code text without waiting for a retry timer.
#[test]
fn queue_reregistration_log_redacts_rejection_code() {
    let error = ApiError::rejected_startup_request(
        RejectedOperation::Register,
        400,
        None,
        "bad-code remote-message top-secret",
    );
    let trace = SharedTraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        log_queue_registration_failure(&error);
    });

    let output = String::from_utf8(trace.bytes()).expect("UTF-8 trace output");
    assert!(
        output
            .contains("category=\"Zulip rejected the request (register, HTTP 400, code unknown)\"")
    );
    for secret in ["bad-code", "remote-message", "top-secret"] {
        assert!(!output.contains(secret), "log leaked `{secret}`");
    }
}

/// Edit, delete, and reaction events reference only a previously admitted owned
/// message and deletion revokes its route.
#[test]
fn mutations_emit_immutable_reports_and_delete_revokes() {
    let (ext, rx, _) = extension();
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    ext.process_event(serde_json::json!({"id":20,"type":"message","message":{"id":600,"type":"private","sender_id":42,"content":"base","flags":[],"display_recipient":[{"id":42},{"id":99}]}}), generation, registration, 99, Some("Tau Bot"));
    let _ = rx.recv().expect("base report");
    ext.process_event(serde_json::json!({"id":21,"type":"update_message","message_id":600,"user_id":42,"message":{"content":"edited"}}), generation, registration, 99, Some("Tau Bot"));
    ext.process_event(serde_json::json!({"id":22,"type":"reaction","message_id":600,"user_id":42,"op":"add","emoji_name":"thumbs_up"}), generation, registration, 99, Some("Tau Bot"));
    ext.process_event(
        serde_json::json!({"id":23,"type":"delete_message","message_id":600,"user_id":42}),
        generation,
        registration,
        99,
        Some("Tau Bot"),
    );
    assert!(matches!(
        event_from(rx.recv().expect("edit")),
        Event::MessageEditedReported(_)
    ));
    assert!(matches!(
        event_from(rx.recv().expect("reaction")),
        Event::MessageReactionAddedReported(_)
    ));
    assert!(matches!(
        event_from(rx.recv().expect("delete")),
        Event::MessageDeletedReported(_)
    ));
    assert!(
        !ext.state
            .lock()
            .owners
            .contains_key(message_fact_id(&cfg(), 600).as_str())
    );
}

/// Read exactly one complete HTTP request from a loopback client connection.
fn read_http_request(socket: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 2048];
        let count = socket.read(&mut chunk).expect("read request");
        bytes.extend_from_slice(&chunk[..count]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        if header_end + 4 + content_length <= bytes.len() {
            return String::from_utf8_lossy(&bytes).to_string();
        }
    }
}

/// Return one deterministic `users_me` rejection from a loopback Zulip server.
fn users_me_rejection(status: &str, headers: &str, code: &str) -> ApiError {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Zulip");
    let address = listener.local_addr().expect("address");
    let status = status.to_owned();
    let headers = headers.to_owned();
    let body = serde_json::json!({"result":"error","code":code}).to_string();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept identity request");
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let request = read_http_request(&mut socket);
        assert!(request.starts_with("GET /api/v1/users/me "));
        write!(
            socket,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n{headers}Connection: close\r\n\r\n{body}",
            body.len(),
        )
        .expect("reject identity");
    });
    let mut config = cfg();
    config.api_base = format!("http://{address}/api/v1");
    let error = HttpZulipClient::default()
        .register_queue(&config)
        .expect_err("identity rejection");
    server.join().expect("server");
    error
}

/// The production client fetches its own bounded identity before registering,
/// requests only the realm metadata needed to bound long polls, sends
/// credentials only through Basic authentication, and keeps the identity key
/// and complete realm user directory off the wire.
#[test]
fn http_register_uses_basic_auth_and_native_queue_api() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Zulip");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut identity_socket, _) = listener.accept().expect("accept identity request");
        identity_socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let identity_request = read_http_request(&mut identity_socket);
        assert!(identity_request.starts_with("GET /api/v1/users/me "));
        assert!(
            identity_request
                .to_ascii_lowercase()
                .contains("authorization: basic ym90qgv4yw1wbguudgvzddp0b3atc2vjcmv0")
        );
        let identity_body = r#"{"result":"success","user_id":99,"full_name":"Bridge Bot"}"#;
        write!(identity_socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}", identity_body.len(), identity_body).expect("respond with identity");

        let (mut register_socket, _) = listener.accept().expect("accept register request");
        register_socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let register_request = read_http_request(&mut register_socket);
        assert!(register_request.starts_with("POST /api/v1/register "));
        assert!(
            register_request
                .to_ascii_lowercase()
                .contains("authorization: basic ym90qgv4yw1wbguudgvzddp0b3atc2vjcmv0")
        );
        assert!(register_request.contains("apply_markdown=false"));
        let (_, form) = register_request
            .split_once("\r\n\r\n")
            .expect("request contains header boundary");
        let form: BTreeMap<_, _> = url::form_urlencoded::parse(form.as_bytes())
            .into_owned()
            .collect();
        assert!(
            form.get("fetch_event_types")
                .is_some_and(|value| value == "[\"realm\"]"),
            "registration must request the `realm` section containing the \
             long-poll timeout"
        );
        assert_eq!(
            form.get("client_capabilities").map(String::as_str),
            Some(r#"{"empty_topic_name":true}"#),
            "registration must preserve Zulip's canonical empty general-chat topic"
        );
        assert!(
            !form.values().any(|value| value.contains("realm_user")),
            "registration must not load the complete realm user directory"
        );
        assert!(
            !register_request
                .lines()
                .next()
                .expect("request line")
                .contains("top-secret")
        );
        let register_body = r#"{"result":"success","queue_id":"opaque-q","last_event_id":5,"event_queue_longpoll_timeout_seconds":90}"#;
        write!(register_socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}", register_body.len(), register_body).expect("respond with queue");
        (identity_request, register_request)
    });
    let mut config = cfg();
    config.api_base = format!("http://{address}/api/v1");
    let queue = HttpZulipClient::default()
        .register_queue(&config)
        .expect("register queue");
    assert_eq!(queue.queue_id, "opaque-q");
    assert_eq!(queue.bot_user_id, 99);
    assert_eq!(queue.bot_full_name.as_deref(), Some("Bridge Bot"));
    let (identity_request, register_request) = server.join().expect("server");
    assert!(!identity_request.contains("queue-secret"));
    assert!(!register_request.contains("queue-secret"));
}

/// A rejected authenticated-user lookup reports only its stable operation,
/// status, and machine code, never remote text or configured credentials.
#[test]
fn users_me_rejection_diagnostic_is_content_free() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Zulip");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept identity request");
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let request = read_http_request(&mut socket);
        assert!(request.starts_with("GET /api/v1/users/me "));
        let body = r#"{"result":"error","code":"INVALID_API_KEY","msg":"remote-message top-secret bot@example.test","reflected_body":"register payload","url":"https://top-secret@chat.example.test/path?api_key=top-secret","response_header":"X-Remote-Token: top-secret"}"#;
        write!(
            socket,
            "HTTP/1.1 418 Teapot\r\nContent-Length: {}\r\nContent-Type: application/json\r\nX-Remote-Token: top-secret\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("reject identity");
    });
    let mut config = cfg();
    config.api_base = format!("http://{address}/api/v1");

    let error = HttpZulipClient::default()
        .register_queue(&config)
        .expect_err("identity rejection");
    let diagnostic = error.diagnostic();

    assert_eq!(
        diagnostic,
        "Zulip rejected the request (users_me, HTTP 418, code INVALID_API_KEY)"
    );
    for secret in [
        "remote-message",
        "top-secret",
        "bot@example.test",
        "register payload",
        "chat.example.test",
        "X-Remote-Token",
    ] {
        assert!(!diagnostic.contains(secret), "diagnostic leaked `{secret}`");
    }
    server.join().expect("server");
}

/// Every HTTP failure category retains its established diagnostic prefix while
/// adding only sanitized startup-rejection metadata and a bounded retry delay.
#[test]
fn users_me_http_rejections_preserve_categories_and_bounds() {
    let forbidden = users_me_rejection("403 Forbidden", "", "BAD_PERMISSION");
    assert!(matches!(forbidden, ApiError::Unauthorized { .. }));
    assert_eq!(
        forbidden.diagnostic(),
        "Zulip authentication failed (users_me, HTTP 403, code BAD_PERMISSION)"
    );

    let rate_limited = users_me_rejection("429 Too Many Requests", "", "RATE_LIMITED");
    assert!(matches!(
        rate_limited,
        ApiError::RateLimited {
            retry,
            ..
        } if retry == Duration::from_secs(1)
    ));
    assert_eq!(
        rate_limited.diagnostic(),
        "Zulip rate limit exceeded (users_me, HTTP 429, code RATE_LIMITED)"
    );

    let capped_rate_limit = users_me_rejection(
        "429 Too Many Requests",
        "Retry-After: 99\r\n",
        "RATE_LIMITED",
    );
    assert!(matches!(
        capped_rate_limit,
        ApiError::RateLimited {
            retry,
            ..
        } if retry == Duration::from_secs(30)
    ));

    let unavailable = users_me_rejection("503 Service Unavailable", "", "SERVER_ERROR");
    assert!(matches!(unavailable, ApiError::Unavailable { .. }));
    assert_eq!(
        unavailable.diagnostic(),
        "Zulip service is unavailable (users_me, HTTP 503, code SERVER_ERROR)"
    );

    let malformed_code = users_me_rejection("400 Bad Request", "", "bad-code secret");
    assert!(matches!(malformed_code, ApiError::InvalidRequest { .. }));
    assert_eq!(
        malformed_code.diagnostic(),
        "Zulip rejected the request (users_me, HTTP 400, code unknown)"
    );
}

/// A rejected queue registration reports its stable operation and substitutes
/// `unknown` for an oversized remote machine code without exposing the body.
#[test]
fn register_rejection_diagnostic_bounds_remote_code() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Zulip");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut identity_socket, _) = listener.accept().expect("accept identity request");
        identity_socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let identity_request = read_http_request(&mut identity_socket);
        assert!(identity_request.starts_with("GET /api/v1/users/me "));
        let identity_body = r#"{"result":"success","user_id":99}"#;
        write!(
            identity_socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            identity_body.len(),
            identity_body
        )
        .expect("respond with identity");

        let (mut register_socket, _) = listener.accept().expect("accept register request");
        register_socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let register_request = read_http_request(&mut register_socket);
        assert!(register_request.starts_with("POST /api/v1/register "));
        let oversized_code = format!("BAD_{}", "A".repeat(128));
        let body = serde_json::json!({
            "result": "error",
            "code": oversized_code,
            "msg": "remote-message top-secret bot@example.test",
            "reflected_body": "event_types payload",
            "url": "https://top-secret@chat.example.test/path?api_key=top-secret",
            "response_header": "X-Remote-Token: top-secret",
        })
        .to_string();
        write!(
            register_socket,
            "HTTP/1.1 422 Unprocessable Content\r\nContent-Length: {}\r\nContent-Type: application/json\r\nX-Remote-Token: top-secret\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("reject registration");
    });
    let mut config = cfg();
    config.api_base = format!("http://{address}/api/v1");

    let error = HttpZulipClient::default()
        .register_queue(&config)
        .expect_err("registration rejection");
    let diagnostic = error.diagnostic();

    assert_eq!(
        diagnostic,
        "Zulip rejected the request (register, HTTP 422, code unknown)"
    );
    for secret in [
        "remote-message",
        "top-secret",
        "bot@example.test",
        "event_types payload",
        "chat.example.test",
        "X-Remote-Token",
    ] {
        assert!(!diagnostic.contains(secret), "diagnostic leaked `{secret}`");
    }
    server.join().expect("server");
}

/// A semantic Zulip error in a successful HTTP response keeps the 200 status
/// and accepts exactly 64 safe code bytes without retaining the response body.
#[test]
fn register_semantic_rejection_keeps_status_and_maximum_code() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Zulip");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut identity_socket, _) = listener.accept().expect("accept identity request");
        identity_socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let identity_request = read_http_request(&mut identity_socket);
        assert!(identity_request.starts_with("GET /api/v1/users/me "));
        let identity_body = r#"{"result":"success","user_id":99}"#;
        write!(
            identity_socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            identity_body.len(),
            identity_body
        )
        .expect("respond with identity");

        let (mut register_socket, _) = listener.accept().expect("accept register request");
        register_socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let register_request = read_http_request(&mut register_socket);
        assert!(register_request.starts_with("POST /api/v1/register "));
        let code = "A".repeat(64);
        let body = serde_json::json!({"result":"error","code":code}).to_string();
        write!(
            register_socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("reject registration");
    });
    let mut config = cfg();
    config.api_base = format!("http://{address}/api/v1");

    let error = HttpZulipClient::default()
        .register_queue(&config)
        .expect_err("semantic registration rejection");

    assert_eq!(
        error.diagnostic(),
        format!(
            "Zulip rejected the request (register, HTTP 200, code {})",
            "A".repeat(64)
        )
    );
    server.join().expect("server");
}

/// An oversized response still preserves the known startup rejection category,
/// operation, and HTTP status while replacing its unknown machine code safely.
#[test]
fn users_me_oversized_rejection_preserves_category() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Zulip");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept identity request");
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let request = read_http_request(&mut socket);
        assert!(request.starts_with("GET /api/v1/users/me "));
        let body = "x".repeat(MAX_API_RESPONSE_BYTES as usize + 1);
        write!(
            socket,
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write oversized rejection");
    });
    let mut config = cfg();
    config.api_base = format!("http://{address}/api/v1");

    let error = HttpZulipClient::default()
        .register_queue(&config)
        .expect_err("oversized identity rejection");

    assert_eq!(
        error.diagnostic(),
        "Zulip authentication failed (users_me, HTTP 401, code unknown)"
    );
    server.join().expect("server");
}

/// Registration returns the fixed-shape remote-rejection diagnostic through
/// the public tool result rather than exposing a raw provider error.
#[test]
fn register_tool_returns_content_free_rejection_diagnostic() {
    let (ext, _rx, client) = extension();
    client
        .register_error
        .lock()
        .expect("register lock")
        .replace(ApiError::rejected_startup_request(
            RejectedOperation::Register,
            400,
            None,
            "BAD_REQUEST",
        ));
    ext.state.lock().queue = None;

    let result = ext.handle_register(
        tool(REGISTER_TOOL_NAME, vec![("enabled", CborValue::Bool(true))]),
        Some(1),
    );

    let Event::ToolError(error) = result else {
        panic!("expected registration error")
    };
    assert_eq!(
        error.message,
        "Zulip rejected the request (register, HTTP 400, code BAD_REQUEST)"
    );
}

/// The production client long-polls the native events endpoint with an opaque
/// queue cursor and decodes bounded event arrays.
#[test]
fn http_event_poll_uses_queue_cursor() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Zulip");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let mut bytes = Vec::new();
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0; 2048];
            let count = socket.read(&mut chunk).expect("read request");
            bytes.extend_from_slice(&chunk[..count]);
        }
        let request = String::from_utf8_lossy(&bytes).to_string();
        assert!(
            request.starts_with(
                "GET /api/v1/events?queue_id=opaque-q&last_event_id=5&dont_block=false "
            )
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: basic ")
        );
        let body = serde_json::json!({
            "result": "success",
            "events": (6..263)
                .map(|id| serde_json::json!({"id": id, "type": "heartbeat"}))
                .collect::<Vec<_>>(),
        })
        .to_string();
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("respond");
    });
    let mut config = cfg();
    config.api_base = format!("http://{address}/api/v1");
    let events = HttpZulipClient::default()
        .get_events(&config, "opaque-q", 5, Duration::from_secs(100))
        .expect("poll events");
    assert_eq!(events.len(), 257);
    server.join().expect("server");
}
