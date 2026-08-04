use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex, mpsc};

use super::*;
use crate::api::{SentMessage, ZulipClient};

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
        Err(ApiError::Unavailable)
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
            proactive_send: true,
            description: Some("Operations".to_owned()),
        }],
        receive_direct_messages: true,
        max_message_bytes: 1024,
        id_key: [7; 32],
    }
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
    let (tx, rx) = mpsc::channel();
    let client = Arc::new(FakeClient::default());
    let ext = Extension::new(client.clone(), tx, ToolNames::logical());
    ext.apply_config(cfg(), publisher());
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
