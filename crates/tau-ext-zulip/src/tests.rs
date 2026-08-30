use std::collections::BTreeMap;
use std::io::{Cursor, Error};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, mpsc};

use tau_proto::{HarnessInputMessage, HarnessOutputMessage};

use super::*;
use crate::api::{MessagePage, RejectedOperation, SentMessage, ZulipClient};
use crate::config::ConfiguredStreamRoute;

static SATURATION_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Clears the correlated production saturation hook after each test.
struct SaturationHookGuard;

impl Drop for SaturationHookGuard {
    fn drop(&mut self) {
        SATURATION_HOOK
            .lock()
            .expect("zulip saturation hook")
            .take();
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

impl std::io::Write for SaturationWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
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

    fn flush(&mut self) -> std::io::Result<()> {
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
    /// Matching frames to accept before failing the selected one.
    skip_matches: usize,
}

impl std::io::Write for FailingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes
            .windows(self.target.len())
            .any(|window| window == self.target)
        {
            if 0 < self.skip_matches {
                self.skip_matches -= 1;
                self.bytes
                    .lock()
                    .expect("output bytes")
                    .extend_from_slice(bytes);
                return Ok(bytes.len());
            }
            if let Some(failed) = self.failed.take() {
                let _ = failed.send(());
            }
            return Err(Error::other("forced Zulip writer failure"));
        }
        self.bytes
            .lock()
            .expect("output bytes")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

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
    /// Next configured-channel resolution outcome.
    resolve_error: Mutex<Option<ApiError>>,
    /// Next all-message subscription outcome.
    subscribe_error: Mutex<Option<ApiError>>,
    /// Optional per-name native IDs used for collision tests.
    resolved_stream_ids: Mutex<HashMap<String, u64>>,
    /// Ordered route-resolution, subscription, and registration calls.
    queue_setup_calls: Mutex<Vec<&'static str>>,
    /// All channel-name batches submitted for bot subscription.
    subscriptions: Mutex<Vec<Vec<String>>>,
    /// Sent route and content observations.
    sends: Mutex<Vec<(NativeRoute, String)>>,
    /// Reaction observations.
    reactions: Mutex<Vec<(u64, String, bool)>>,
    /// Optional signal emitted when queue registration starts.
    register_started: Mutex<Option<mpsc::Sender<()>>>,
    /// Optional gate delaying queue registration completion.
    register_release: Mutex<Option<mpsc::Receiver<()>>>,
    /// Deterministic visible message history.
    history: Mutex<Vec<serde_json::Value>>,
    /// Events returned by the startup nonblocking queue drain.
    queued_events: Mutex<Vec<serde_json::Value>>,
    /// One event batch returned by the blocking worker poll.
    live_events: Mutex<Option<Vec<serde_json::Value>>>,
    /// Number of currently executing worker polls.
    active_event_polls: AtomicUsize,
    /// Number of queue-worker exits.
    worker_exits: AtomicUsize,
    /// Notification for deterministic queue-worker retirement assertions.
    worker_exit_changed: Condvar,
    /// Mutex paired with the worker-exit condition variable.
    worker_exit_lock: Mutex<()>,
    /// Requested history page sizes and anchors.
    history_requests: Mutex<Vec<(u64, usize)>>,
    /// Optional server completion marker for deterministic pagination tests.
    history_found_newest: Mutex<Option<bool>>,
    /// Exact next history page used for malformed-page tests.
    history_page: Mutex<Option<MessagePage>>,
}

impl ZulipClient for FakeClient {
    fn worker_exited(&self) {
        let _guard = self.worker_exit_lock.lock().expect("worker exit lock");
        self.worker_exits.fetch_add(1, AtomicOrdering::SeqCst);
        self.worker_exit_changed.notify_all();
    }
    fn resolve_stream_id(&self, _cfg: &RuntimeConfig, name: &str) -> Result<u64, ApiError> {
        self.queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .push("resolve");
        if let Some(error) = self.resolve_error.lock().expect("resolve error").take() {
            return Err(error);
        }
        Ok(*self
            .resolved_stream_ids
            .lock()
            .expect("resolved stream IDs")
            .get(name)
            .unwrap_or(&7))
    }

    fn subscribe(&self, _cfg: &RuntimeConfig, names: &[String]) -> Result<(), ApiError> {
        self.queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .push("subscribe");
        self.subscriptions
            .lock()
            .expect("subscriptions")
            .push(names.to_vec());
        if let Some(error) = self.subscribe_error.lock().expect("subscribe error").take() {
            return Err(error);
        }
        Ok(())
    }

    fn register_queue(&self, _cfg: &RuntimeConfig) -> Result<EventQueue, ApiError> {
        self.queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .push("register");
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
        self.active_event_polls.fetch_add(1, AtomicOrdering::SeqCst);
        let _active = ActiveEventPoll(&self.active_event_polls);
        self.live_events
            .lock()
            .expect("live events lock")
            .take()
            .ok_or_else(ApiError::unavailable)
    }

    fn get_events_now(
        &self,
        _cfg: &RuntimeConfig,
        _queue_id: &str,
        _last_event_id: i64,
    ) -> Result<Vec<serde_json::Value>, ApiError> {
        Ok(std::mem::take(
            &mut *self.queued_events.lock().expect("queued events lock"),
        ))
    }

    fn get_messages_after(
        &self,
        _cfg: &RuntimeConfig,
        after: u64,
        limit: usize,
    ) -> Result<MessagePage, ApiError> {
        self.history_requests
            .lock()
            .expect("history requests lock")
            .push((after, limit));
        if let Some(page) = self.history_page.lock().expect("history page lock").take() {
            return Ok(page);
        }
        let messages = self
            .history
            .lock()
            .expect("history lock")
            .iter()
            .filter(|message| {
                message
                    .get("id")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|id| after < id)
            })
            .take(limit)
            .cloned()
            .collect();
        Ok(MessagePage {
            messages,
            found_newest: self
                .history_found_newest
                .lock()
                .expect("history completion lock")
                .unwrap_or(true),
        })
    }

    fn newest_message_id(&self, _cfg: &RuntimeConfig) -> Result<Option<u64>, ApiError> {
        Ok(self
            .history
            .lock()
            .expect("history lock")
            .iter()
            .filter_map(|message| message.get("id").and_then(serde_json::Value::as_u64))
            .max())
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

/// Decrements the active provider-call count on every return path.
struct ActiveEventPoll<'a>(&'a AtomicUsize);

impl Drop for ActiveEventPoll<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, AtomicOrdering::SeqCst);
    }
}

/// Channel names resolve to native routing IDs before an all-message
/// subscription, so queue registration cannot expose a broad receive route.
#[test]
fn all_message_channel_subscription_precedes_queue_registration() {
    let config = validated_config(serde_json::json!([{
        "name": "Engineering",
        "receive": "all_messages"
    }]))
    .expect("valid all-message channel");
    let client = Arc::new(FakeClient::default());
    let ext = Extension::new(client.clone(), mpsc::channel().0, ToolNames::logical());

    let (resolved, _queue) = ext.acquire_queue(&config).expect("queue setup");

    assert_eq!(resolved.routes[0].stream_id, 7);
    assert_eq!(
        client
            .queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .as_slice(),
        &["resolve", "subscribe", "register"]
    );
    assert_eq!(
        client
            .subscriptions
            .lock()
            .expect("subscriptions")
            .as_slice(),
        &[vec!["Engineering".to_owned()]]
    );
}

/// Mention-only channels still resolve private routing IDs but must not create
/// subscriptions, preserving their operator-managed membership boundary.
#[test]
fn mention_only_channel_does_not_subscribe() {
    let config = validated_config(serde_json::json!([{
        "name": "Operations",
        "topic": "deploy",
        "receive": "mentions_only"
    }]))
    .expect("valid mention-only channel");
    let client = Arc::new(FakeClient::default());
    let ext = Extension::new(client.clone(), mpsc::channel().0, ToolNames::logical());

    let (resolved, _queue) = ext.acquire_queue(&config).expect("queue setup");

    assert_eq!(resolved.routes[0].stream_id, 7);
    assert_eq!(
        client
            .queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .as_slice(),
        &["resolve", "register"]
    );
    assert!(
        client
            .subscriptions
            .lock()
            .expect("subscriptions")
            .is_empty()
    );
}

/// Resolution or subscription failure must stop before a queue installs an
/// unresolved route or broadens the all-message receive surface.
#[test]
fn queue_setup_failure_prevents_registration() {
    let config = validated_config(serde_json::json!([{
        "name": "Engineering",
        "receive": "all_messages"
    }]))
    .expect("valid all-message channel");
    let client = Arc::new(FakeClient::default());
    let ext = Extension::new(client.clone(), mpsc::channel().0, ToolNames::logical());
    *client.resolve_error.lock().expect("resolve error") = Some(ApiError::unavailable());
    assert!(ext.acquire_queue(&config).is_err());
    assert_eq!(
        client
            .queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .as_slice(),
        &["resolve"]
    );

    client
        .queue_setup_calls
        .lock()
        .expect("queue setup calls")
        .clear();
    *client.subscribe_error.lock().expect("subscribe error") = Some(ApiError::unavailable());
    assert!(ext.acquire_queue(&config).is_err());
    assert_eq!(
        client
            .queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .as_slice(),
        &["resolve", "subscribe"]
    );
}

/// Distinct configured names that resolve to one overlapping native channel
/// must fail before subscription or queue registration makes routing ambiguous.
#[test]
fn colliding_resolved_receive_routes_fail_before_queue_registration() {
    let config = validated_config(serde_json::json!([
        {"name":"Engineering", "topic":"deploy", "receive":"all_messages"},
        {"name":"Engineering mirror", "topic":"deploy", "receive":"mentions_only"}
    ]))
    .expect("distinct configured names");
    let client = Arc::new(FakeClient::default());
    client
        .resolved_stream_ids
        .lock()
        .expect("resolved stream IDs")
        .extend([
            ("Engineering".to_owned(), 7),
            ("Engineering mirror".to_owned(), 7),
        ]);
    let ext = Extension::new(client.clone(), mpsc::channel().0, ToolNames::logical());

    assert!(ext.acquire_queue(&config).is_err());
    assert_eq!(
        client
            .queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .as_slice(),
        &["resolve", "resolve"]
    );
}

/// Each queue replacement must resolve names and repeat the idempotent
/// all-message subscription before registering the replacement queue.
#[test]
fn queue_recovery_repeats_resolution_subscription_and_registration() {
    let config = validated_config(serde_json::json!([{
        "name": "Engineering",
        "receive": "all_messages"
    }]))
    .expect("valid all-message channel");
    let client = Arc::new(FakeClient::default());
    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(client.clone(), tx, ToolNames::logical());
    ext.apply_config(config.clone(), publisher());
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);

    handle_queue_expiry(&ext, &config, generation, registration);
    handle_queue_expiry(&ext, &config, generation, registration);

    assert_eq!(
        client
            .queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .as_slice(),
        &[
            "resolve",
            "subscribe",
            "register",
            "resolve",
            "subscribe",
            "register"
        ]
    );
}

fn cfg() -> RuntimeConfig {
    RuntimeConfig {
        mode: BridgeMode::Receive,
        email: "bot@example.test".to_owned(),
        api_key: "top-secret".to_owned(),
        api_base: "http://127.0.0.1:1/api/v1".to_owned(),
        allowed_user_ids: HashSet::from([42]),
        sender_aliases: HashMap::from([(42, "alice".to_owned())]),
        configured_routes: vec![ConfiguredStreamRoute {
            name: "ops".to_owned(),
            topic: Some("deploy".to_owned()),
            receive: Some(ReceiveMode::MentionsOnly),
            proactive: ProactiveRoute::ExactTopic("deploy".to_owned()),
            description: Some("Operations".to_owned()),
        }],
        routes: vec![StreamRoute {
            name: "ops".to_owned(),
            stream_id: 7,
            topic: Some("deploy".to_owned()),
            receive: Some(ReceiveMode::MentionsOnly),
            proactive: ProactiveRoute::ExactTopic("deploy".to_owned()),
            description: Some("Operations".to_owned()),
        }],
        direct_routes: Vec::new(),
        receive_direct_messages: true,
        max_message_bytes: 1024,
        id_key: [7; 32],
        offline_message_catch_up: false,
        state_dir: None,
    }
}

/// Validate a minimal stream-route configuration with deterministic test
/// secrets.
fn validated_config(conversations: serde_json::Value) -> Result<RuntimeConfig, String> {
    validated_config_with_direct_messages(conversations, serde_json::json!([]))
}

/// Validate stream and proactive direct-message routes with deterministic test
/// secrets.
fn validated_config_with_direct_messages(
    conversations: serde_json::Value,
    proactive_direct_messages: serde_json::Value,
) -> Result<RuntimeConfig, String> {
    let config = CborValue::serialized(&serde_json::json!({
        "bot_email_secret":"email",
        "api_key_secret":"key",
        "identity_key_secret":"identity",
        "site":"https://chat.example.test",
        "allowed_user_ids":[42],
        "conversations":conversations,
        "proactive_direct_messages":proactive_direct_messages,
        "direct_messages":{"receive":"all_messages"},
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

/// Validate the deliberately narrow fixed-recipient mode without any inbound
/// sender or route authority.
fn validated_send_only_config() -> Result<RuntimeConfig, String> {
    let config = CborValue::serialized(&serde_json::json!({
        "send_only":true,
        "bot_email_secret":"email",
        "api_key_secret":"key",
        "identity_key_secret":"identity",
        "site":"https://chat.example.test",
        "proactive_direct_messages":[{
            "alias":"dpc",
            "recipient":1180954,
            "description":"Operator escalation"
        }],
        "max_message_bytes":4096
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

/// Catch-up must remain opt-in so existing configurations preserve live-only
/// behavior without acquiring persistent state.
#[test]
fn offline_message_catch_up_defaults_to_disabled() {
    let config = validated_config(serde_json::json!([{
        "name": "ops",
        "topic": "deploy",
        "receive": "all_messages"
    }]))
    .expect("valid config");
    assert!(!config.offline_message_catch_up);
    assert!(config.state_dir.is_none());
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
    mut config: RuntimeConfig,
) -> (
    Extension,
    mpsc::Receiver<HarnessInputMessage>,
    Arc<FakeClient>,
) {
    config.routes = config
        .configured_routes
        .iter()
        .map(|route| route.resolve(7))
        .collect();
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
            poll_request_timeout: Duration::from_secs(100),
        });
        state.registration_generation = ZulipRegistrationGeneration::new(1);
    }
    (ext, rx, client)
}

/// First use establishes a baseline without replay; a later bounded history
/// page waits for the canonical self-echo before advancing past an admitted
/// message while safely completing a filtered successor.
#[test]
fn offline_catch_up_baselines_filters_and_advances_on_canonical_echo() {
    let directory = tempfile::tempdir().expect("state directory");
    let mut config = cfg();
    config.offline_message_catch_up = true;
    config.state_dir = Some(directory.path().to_path_buf());
    let (ext, rx, client) = extension_with_config(config.clone());
    client
        .history
        .lock()
        .expect("history lock")
        .push(serde_json::json!({
            "id": 10, "sender_id": 42, "content": "old",
            "type": "stream", "stream_id": 7, "subject": "deploy",
            "flags": ["mentioned"]
        }));
    ext.state.lock().checkpoint =
        Some(CheckpointRuntime::open(directory.path(), &config.id_key).expect("checkpoint"));
    let queue = ext.state.lock().queue.clone().expect("queue");
    ext.catch_up_messages(
        &config,
        &queue,
        ZulipConfigGeneration::new(1),
        ZulipRegistrationGeneration::new(1),
    )
    .expect("baseline");
    assert!(rx.try_recv().is_err(), "baseline must not replay history");
    assert_eq!(
        ext.state
            .lock()
            .checkpoint
            .as_ref()
            .expect("checkpoint")
            .position(),
        Some(10)
    );

    client.history.lock().expect("history lock").extend([
        serde_json::json!({
            "id": 11, "sender_id": 42, "content": "allowed",
            "type": "stream", "stream_id": 7, "subject": "deploy",
            "flags": ["mentioned"]
        }),
        serde_json::json!({
            "id": 12, "sender_id": 77, "content": "blocked",
            "type": "stream", "stream_id": 7, "subject": "deploy",
            "flags": ["mentioned"]
        }),
    ]);
    ext.state
        .lock()
        .checkpoint
        .as_mut()
        .expect("checkpoint")
        .set_more_history(true);
    ext.catch_up_messages(
        &config,
        &queue,
        ZulipConfigGeneration::new(1),
        ZulipRegistrationGeneration::new(1),
    )
    .expect("catch-up");
    let HarnessInputMessage::Emit(report) = rx.recv().expect("admitted report") else {
        panic!("expected report");
    };
    let Event::MessageDeliveredReported(delivered) = *report.event else {
        panic!("expected delivered report");
    };
    assert_eq!(delivered.text, "allowed");
    assert_eq!(
        ext.state
            .lock()
            .checkpoint
            .as_ref()
            .expect("checkpoint")
            .position(),
        Some(10),
        "filtered successor must not skip an unacknowledged admitted message"
    );

    let runtime = ZulipRuntime { ext };
    handle_live_event(
        &runtime,
        Event::MessageDelivered(
            delivered.with_publisher(
                tau_proto::MessagePublisherId::parse("std-zulip").expect("publisher"),
            ),
        ),
    );
    assert_eq!(
        runtime
            .ext
            .state
            .lock()
            .checkpoint
            .as_ref()
            .expect("checkpoint")
            .position(),
        Some(12)
    );
    assert_eq!(
        client
            .history_requests
            .lock()
            .expect("history requests lock")
            .last(),
        Some(&(10, 100))
    );
}

/// Partial unregister retains checkpoint-owned echo correlation independently
/// of reply-owner eviction, while last unregister releases the identity lock.
#[test]
fn unregister_preserves_pending_echo_and_releases_last_owner_lock() {
    let directory = tempfile::tempdir().expect("state directory");
    let mut state = State::default();
    let first = agent_id();
    let second = AgentId::parse("second").expect("second agent");
    state
        .registered_agents
        .extend([first.clone(), second.clone()]);
    let mut checkpoint =
        CheckpointRuntime::open(directory.path(), &[9; 32]).expect("checkpoint runtime");
    assert!(checkpoint.begin(20));
    let fact_id = MessageFactId::new("pending-fact");
    checkpoint.submitted(20, fact_id.clone());
    state.checkpoint = Some(checkpoint);

    state.unregister_agent(&first);
    let checkpoint = state.checkpoint.as_mut().expect("partial state retained");
    assert!(checkpoint.acknowledge(&fact_id));
    checkpoint.advance().expect("advance after owner eviction");
    assert_eq!(checkpoint.position(), Some(20));

    state.unregister_agent(&second);
    assert!(state.checkpoint.is_none());
    assert!(
        CheckpointRuntime::open(directory.path(), &[9; 32]).is_ok(),
        "last unregister must release the identity-scoped lock"
    );
}

/// A closed extension-to-harness writer must retain the failed created message
/// as an ordered retry barrier, including before a first-use baseline.
#[test]
fn failed_report_submission_blocks_baseline_checkpoint() {
    let directory = tempfile::tempdir().expect("state directory");
    let mut config = cfg();
    config.offline_message_catch_up = true;
    config.state_dir = Some(directory.path().to_path_buf());
    let (tx, rx) = mpsc::channel();
    drop(rx);
    let ext = Extension::new(Arc::new(FakeClient::default()), tx, ToolNames::logical());
    ext.apply_config(config.clone(), publisher());
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id());
        state.queue = Some(EventQueue {
            queue_id: "queue".to_owned(),
            last_event_id: 0,
            bot_user_id: 99,
            poll_request_timeout: Duration::from_secs(1),
        });
        state.registration_generation = ZulipRegistrationGeneration::new(1);
        state.checkpoint =
            Some(CheckpointRuntime::open(directory.path(), &config.id_key).expect("checkpoint"));
    }
    ext.observe_created_message(
        serde_json::json!({
            "id": 11, "type": "message", "message": {
                "id": 11, "sender_id": 42, "content": "retry me",
                "type": "stream", "stream_id": 7, "subject": "deploy",
                "flags": ["mentioned"]
            }
        }),
        ZulipConfigGeneration::new(1),
        ZulipRegistrationGeneration::new(1),
        99,
    );
    let mut state = ext.state.lock();
    let checkpoint = state.checkpoint.as_mut().expect("checkpoint");
    assert_eq!(checkpoint.retry_position(), Some(11));
    checkpoint.baseline(12);
    checkpoint.advance().expect("blocked no-op");
    assert_eq!(checkpoint.position(), None);
}

/// One activation fetches at most one 100-message page and preserves the
/// server's unfinished marker for the next bounded activation.
#[test]
fn catch_up_pagination_is_bounded_and_resumable() {
    let directory = tempfile::tempdir().expect("state directory");
    let mut config = cfg();
    config.offline_message_catch_up = true;
    config.state_dir = Some(directory.path().to_path_buf());
    let (ext, _rx, client) = extension_with_config(config.clone());
    client
        .history
        .lock()
        .expect("history lock")
        .push(serde_json::json!({
            "id": 1, "sender_id": 77, "content": "filtered",
            "type": "stream", "stream_id": 7, "subject": "deploy",
            "flags": ["mentioned"]
        }));
    *client
        .history_found_newest
        .lock()
        .expect("history completion lock") = Some(false);
    let mut checkpoint =
        CheckpointRuntime::open(directory.path(), &config.id_key).expect("checkpoint");
    checkpoint.baseline(0);
    checkpoint.advance().expect("initial position");
    ext.state.lock().checkpoint = Some(checkpoint);
    let queue = ext.state.lock().queue.clone().expect("queue");

    ext.catch_up_messages(
        &config,
        &queue,
        ZulipConfigGeneration::new(1),
        ZulipRegistrationGeneration::new(1),
    )
    .expect("bounded page");
    assert_eq!(
        client
            .history_requests
            .lock()
            .expect("history requests lock")
            .as_slice(),
        &[(0, 100)]
    );
    let state = ext.state.lock();
    let checkpoint = state.checkpoint.as_ref().expect("checkpoint");
    assert_eq!(checkpoint.position(), Some(1));
    assert!(checkpoint.catch_up_needed());
}

/// A terminal page still must contain strictly increasing numeric IDs; the
/// completion marker cannot turn malformed provider data into a skipped gap.
#[test]
fn catch_up_rejects_malformed_terminal_page() {
    let directory = tempfile::tempdir().expect("state directory");
    let mut config = cfg();
    config.offline_message_catch_up = true;
    config.state_dir = Some(directory.path().to_path_buf());
    let (ext, _rx, client) = extension_with_config(config.clone());
    *client.history_page.lock().expect("history page lock") = Some(MessagePage {
        messages: vec![serde_json::json!({"id": "not-numeric"})],
        found_newest: true,
    });
    let mut checkpoint =
        CheckpointRuntime::open(directory.path(), &config.id_key).expect("checkpoint");
    checkpoint.baseline(0);
    checkpoint.advance().expect("initial position");
    ext.state.lock().checkpoint = Some(checkpoint);
    let queue = ext.state.lock().queue.clone().expect("queue");
    assert!(matches!(
        ext.catch_up_messages(
            &config,
            &queue,
            ZulipConfigGeneration::new(1),
            ZulipRegistrationGeneration::new(1),
        ),
        Err(ApiError::MalformedResponse)
    ));
    assert_eq!(
        ext.state
            .lock()
            .checkpoint
            .as_ref()
            .expect("checkpoint")
            .position(),
        Some(0)
    );
}

/// A message visible in both the first baseline history snapshot and the
/// already-registered live queue is delivered once rather than skipped or
/// replayed twice.
#[test]
fn first_baseline_merges_startup_live_history_overlap() {
    let directory = tempfile::tempdir().expect("state directory");
    let mut config = cfg();
    config.offline_message_catch_up = true;
    config.state_dir = Some(directory.path().to_path_buf());
    let (ext, rx, client) = extension_with_config(config.clone());
    let message = serde_json::json!({
        "id": 11, "sender_id": 42, "content": "during startup",
        "type": "stream", "stream_id": 7, "subject": "deploy",
        "flags": ["mentioned"]
    });
    client.history.lock().expect("history lock").extend([
        serde_json::json!({
            "id": 10, "sender_id": 42, "content": "old history",
            "type": "stream", "stream_id": 7, "subject": "deploy",
            "flags": ["mentioned"]
        }),
        message.clone(),
    ]);
    client
        .queued_events
        .lock()
        .expect("queued events lock")
        .push(serde_json::json!({"id": 50, "type": "message", "message": message}));
    ext.state.lock().checkpoint =
        Some(CheckpointRuntime::open(directory.path(), &config.id_key).expect("checkpoint"));
    let queue = ext.state.lock().queue.clone().expect("queue");
    ext.catch_up_messages(
        &config,
        &queue,
        ZulipConfigGeneration::new(1),
        ZulipRegistrationGeneration::new(1),
    )
    .expect("baseline merge");

    let HarnessInputMessage::Emit(report) = rx.recv().expect("startup live report") else {
        panic!("expected report");
    };
    let Event::MessageDeliveredReported(delivered) = *report.event else {
        panic!("expected delivered report");
    };
    assert_eq!(delivered.text, "during startup");
    assert!(rx.try_recv().is_err(), "history overlap must deduplicate");
}

/// Lost and spurious notifications must not release acknowledgement
/// backpressure; the correlated echo changes the lock-held predicate and wakes
/// the worker without polling.
#[test]
fn checkpoint_wait_uses_echo_predicate_without_polling() {
    let directory = tempfile::tempdir().expect("state directory");
    let (ext, _rx, _client) = extension_with_config(cfg());
    let ext = Arc::new(ext);
    let fact_id = MessageFactId::new("waited-fact");
    let mut checkpoint = CheckpointRuntime::open(directory.path(), &[12; 32]).expect("checkpoint");
    assert!(checkpoint.begin(1));
    checkpoint.submitted(1, fact_id.clone());
    ext.state.lock().checkpoint = Some(checkpoint);

    let held = ext.state.lock();
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = Arc::clone(&ext);
    let thread = std::thread::spawn(move || {
        started_tx.send(()).expect("started");
        worker.wait_for_checkpoint_progress(
            ZulipConfigGeneration::new(1),
            ZulipRegistrationGeneration::new(1),
        );
        done_tx.send(()).expect("done");
    });
    started_rx.recv().expect("waiter started");
    ext.state.changed.notify_all();
    drop(held);
    assert!(
        done_rx.recv_timeout(Duration::from_millis(20)).is_err(),
        "a lost or spurious notification must not bypass the pending echo"
    );

    let mut state = ext.state.lock();
    assert!(
        state
            .checkpoint
            .as_mut()
            .expect("checkpoint")
            .acknowledge(&fact_id)
    );
    ext.state.changed.notify_all();
    drop(state);
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("echo wakes waiter");
    thread.join().expect("waiter");
}

fn tool(name: &str, fields: Vec<(&str, CborValue)>) -> ToolStarted {
    ToolStarted {
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
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

/// Send-only validation rejects every mixed inbound surface and requires one
/// fixed proactive DM, preventing an operator typo from creating receive
/// authority.
#[test]
fn send_only_configuration_rejects_mixed_authority() {
    assert!(
        validated_send_only_config()
            .expect("valid send-only")
            .mode
            .is_send_only()
    );
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
    let base = serde_json::json!({
        "send_only":true,
        "bot_email_secret":"email",
        "api_key_secret":"key",
        "identity_key_secret":"identity",
        "site":"https://chat.example.test",
        "proactive_direct_messages":[{"alias":"dpc","recipient":1180954}]
    });
    let cases = [
        (
            "allowed_user_ids",
            serde_json::json!([42]),
            "zulip send-only config forbids `allowed_user_ids`",
        ),
        (
            "sender_aliases",
            serde_json::json!([{"user_id":42,"alias":"alice"}]),
            "zulip send-only config forbids `sender_aliases`",
        ),
        (
            "conversations",
            serde_json::json!([{"name":"ops","receive":"all_messages"}]),
            "zulip send-only config forbids `conversations`",
        ),
        (
            "direct_messages",
            serde_json::json!({"receive":"all_messages"}),
            "zulip send-only config forbids `direct_messages`",
        ),
        (
            "offline_message_catch_up",
            serde_json::json!(true),
            "zulip send-only config forbids `offline_message_catch_up`",
        ),
    ];
    for (field, value, expected) in cases {
        let mut mixed = base.clone();
        mixed[field] = value;
        let config = CborValue::serialized(&mixed)
            .expect("config value")
            .deserialized::<ExtConfig>()
            .expect("config schema");
        assert_eq!(config.validate(&secrets).err().as_deref(), Some(expected));
    }
    for routes in [
        serde_json::json!([]),
        serde_json::json!([
            {"alias":"dpc","recipient":1180954},
            {"alias":"other","recipient":2}
        ]),
    ] {
        let mut invalid = base.clone();
        invalid["proactive_direct_messages"] = routes;
        let config = CborValue::serialized(&invalid)
            .expect("config value")
            .deserialized::<ExtConfig>()
            .expect("config schema");
        assert_eq!(
            config.validate(&secrets).err().as_deref(),
            Some("zulip send-only config requires exactly one `proactive_direct_messages` route")
        );
    }
}

/// The send-only startup declaration exposes only the fixed-alias send shape,
/// excluding reply and topic arguments from model-visible authority.
#[test]
fn send_only_tool_schema_is_narrowed_to_fixed_destination() {
    let spec = send_only_spec(&ToolNames::logical());
    assert_eq!(spec.name.as_str(), SEND_TOOL_NAME);
    assert_eq!(
        spec.parameters,
        Some(serde_json::json!({
            "type":"object",
            "properties":{
                "message":{"type":"string"},
                "destination":{"type":"string"}
            },
            "required":["message","destination"],
            "additionalProperties":false
        }))
    );
}

/// Send-only permits one fixed proactive DM without registration while denying
/// queue creation, arbitrary destinations, replies, and every injected inbound
/// create or mutation event.
#[test]
fn send_only_sends_without_registering_and_ignores_all_ingress() {
    let config = validated_send_only_config().expect("valid send-only config");
    let (tx, rx) = mpsc::channel();
    let client = Arc::new(FakeClient::default());
    let ext = Extension::new(client.clone(), tx, ToolNames::logical());
    ext.apply_config(config.clone(), publisher());

    let result = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            (
                "message",
                CborValue::Text("host needs attention".to_owned()),
            ),
            ("destination", CborValue::Text("dpc".to_owned())),
        ],
    ));
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        client.sends.lock().expect("sends").as_slice(),
        &[(
            NativeRoute::Direct(vec![1180954]),
            "host needs attention".to_owned()
        )]
    );
    assert!(matches!(
        event_from(rx.recv().expect("sent report")),
        Event::MessageSentReported(_)
    ));
    assert!(
        ext.state.lock().owners.is_empty(),
        "send-only must install no reply or mutation capability"
    );

    for fields in [
        vec![
            ("message", CborValue::Text("wrong".to_owned())),
            ("destination", CborValue::Text("1180954".to_owned())),
        ],
        vec![
            ("message", CborValue::Text("wrong".to_owned())),
            (
                "reply_to",
                CborValue::Text(message_fact_id(&config, 777).as_str().to_owned()),
            ),
        ],
    ] {
        assert!(matches!(
            ext.handle_send(tool(SEND_TOOL_NAME, fields)),
            Event::ToolError(_)
        ));
    }
    assert!(matches!(
        ext.handle_register(
            tool(REGISTER_TOOL_NAME, vec![("enabled", CborValue::Bool(true))]),
            Some(ZulipRegistrationGeneration::new(0))
        ),
        Event::ToolError(_)
    ));
    assert!(
        client
            .queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .is_empty(),
        "send-only must not resolve, subscribe, or register a queue"
    );
    assert_eq!(client.active_event_polls.load(AtomicOrdering::SeqCst), 0);

    let (generation, registration) = {
        let mut state = ext.state.lock();
        let conversation = direct_conversation(&config, &config.direct_routes[0]);
        state.insert_owner(MessageOwner {
            agent_id: agent_id(),
            fact_id: message_fact_id(&config, 777),
            native_message_id: 777,
            conversation,
        });
        (state.config_generation, state.registration_generation)
    };
    let events = [
        serde_json::json!({"id":1,"type":"message","message":{"id":1,"type":"private","sender_id":42,"content":"DM","display_recipient":[{"id":42},{"id":99}]}}),
        serde_json::json!({"id":2,"type":"message","message":{"id":2,"type":"stream","sender_id":42,"stream_id":7,"subject":"ops","content":"stream"}}),
        serde_json::json!({"id":3,"type":"update_message","message_id":777,"user_id":42,"message":{"content":"edit"}}),
        serde_json::json!({"id":4,"type":"delete_message","message_id":777,"user_id":42}),
        serde_json::json!({"id":5,"type":"reaction","message_id":777,"user_id":42,"op":"add","emoji_name":"eyes"}),
        serde_json::json!({"id":6,"type":"update_message","message_id":777,"message":{"content":"actorless"}}),
        serde_json::json!({"id":7,"type":"delete_message","message_id":777}),
        serde_json::json!({"id":8,"type":"reaction","message_id":777,"op":"add","emoji_name":"eyes"}),
        serde_json::json!({"id":9,"type":"update_message","message_id":777,"user_id":"bad","message":{"content":"edit"}}),
        serde_json::json!({"id":10,"type":"delete_message","message_id":777,"user_id":null}),
        serde_json::json!({"id":11,"type":"reaction","message_id":777,"user_id":{},"op":"add","emoji_name":"eyes"}),
    ];
    for event in events {
        ext.process_event(event, generation, registration, 99);
    }
    assert!(
        rx.try_recv().is_err(),
        "Zulip-originated ingress must publish no activation-producing report"
    );
}

/// Ordinary receive mode requires a present numeric allowlisted actor for every
/// owned edit, delete, and reaction, closing the actor-less mutation bypass.
#[test]
fn ordinary_mutations_reject_missing_actor_identity() {
    let (ext, rx, _) = extension();
    let (generation, registration) = {
        let state = ext.state.lock();
        (state.config_generation, state.registration_generation)
    };
    ext.process_event(
        serde_json::json!({
            "id":20,"type":"message","message":{
                "id":600,"type":"private","sender_id":42,"content":"base","flags":[],
                "display_recipient":[{"id":42},{"id":99}]
            }
        }),
        generation,
        registration,
        99,
    );
    assert!(matches!(
        event_from(rx.recv().expect("base report")),
        Event::MessageDeliveredReported(_)
    ));
    for event in [
        serde_json::json!({"id":21,"type":"update_message","message_id":600,"message":{"content":"edited"}}),
        serde_json::json!({"id":22,"type":"delete_message","message_id":600}),
        serde_json::json!({"id":23,"type":"reaction","message_id":600,"op":"add","emoji_name":"eyes"}),
    ] {
        ext.process_event(event, generation, registration, 99);
    }
    assert!(
        rx.try_recv().is_err(),
        "actor-less owned mutations must publish no agent-targeted report"
    );
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
        "name":"channel", "topic":"deploy",
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
        "name":"channel", "receive":"mentions_only",
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
        "name":"channel",
        "proactive_send":true, "agent_chosen_topic":true
    }))
    .validate(&secrets)
    .expect("valid channel-wide authority");
    assert!(
        config.configured_routes[0]
            .proactive
            .allows_agent_chosen_topic()
    );
    assert!(config.configured_routes[0].topic.is_none());
}

/// Topicless agent-chosen receive routes accept every topic, while native
/// stream resolution allows proactive-only coexistence but rejects overlapping
/// receive authority before queue registration.
#[test]
fn agent_chosen_topic_receive_and_collision_rules_are_explicit() {
    let all_topics = validated_config(serde_json::json!([{
        "name":"channel", "receive":"mentions_only",
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
    );
    assert!(matches!(
        event_from(rx.recv().expect("all-topic receive report")),
        Event::MessageDeliveredReported(_)
    ));

    let proactive_and_exact_receive = validated_config(serde_json::json!([
        {
            "name":"channel-wide",
            "proactive_send":true, "agent_chosen_topic":true
        },
        {
            "name":"deploy-receive", "topic":"deploy",
            "receive":"mentions_only"
        }
    ]))
    .expect("distinct proactive-only and exact receive routes");
    let client = Arc::new(FakeClient::default());
    client
        .resolved_stream_ids
        .lock()
        .expect("resolved stream IDs")
        .extend([
            ("channel-wide".to_owned(), 7),
            ("deploy-receive".to_owned(), 7),
        ]);
    let ext = Extension::new(client.clone(), mpsc::channel().0, ToolNames::logical());
    ext.acquire_queue(&proactive_and_exact_receive)
        .expect("proactive-only route may share the exact receive stream");
    assert_eq!(
        client
            .queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .as_slice(),
        &["resolve", "resolve", "register"]
    );

    let overlapping_receive_routes = validated_config(serde_json::json!([
        {
            "name":"all-topics", "receive":"all_messages",
            "proactive_send":true, "agent_chosen_topic":true
        },
        {
            "name":"deploy-receive", "topic":"deploy",
            "receive":"mentions_only"
        }
    ]))
    .expect("distinct configured route names");
    let client = Arc::new(FakeClient::default());
    client
        .resolved_stream_ids
        .lock()
        .expect("resolved stream IDs")
        .extend([
            ("all-topics".to_owned(), 7),
            ("deploy-receive".to_owned(), 7),
        ]);
    let ext = Extension::new(client.clone(), mpsc::channel().0, ToolNames::logical());
    assert!(ext.acquire_queue(&overlapping_receive_routes).is_err());
    assert_eq!(
        client
            .queue_setup_calls
            .lock()
            .expect("queue setup calls")
            .as_slice(),
        &["resolve", "resolve"]
    );
}

/// Proactive direct-message aliases carry exactly one independently configured
/// outbound recipient and reject zero, arrays, duplicate aliases, and unknown
/// nested fields before any Zulip request can target them.
#[test]
fn proactive_direct_message_configuration_fails_closed() {
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
    let parse = |conversations: serde_json::Value, direct_messages: serde_json::Value| {
        CborValue::serialized(&serde_json::json!({
            "bot_email_secret":"email",
            "api_key_secret":"key",
            "identity_key_secret":"identity",
            "site":"https://chat.example.test",
            "allowed_user_ids":[42],
            "conversations":conversations,
            "proactive_direct_messages":direct_messages,
        }))
        .expect("config value")
        .deserialized::<ExtConfig>()
        .expect("config schema")
    };
    let valid = parse(
        serde_json::json!([{
            "name":"ops", "topic":"deploy", "proactive_send":true
        }]),
        serde_json::json!([{
            "alias":"dpc", "recipient":1180954, "description":"Operator escalation"
        }]),
    )
    .validate(&secrets)
    .expect("outbound recipient need not be an inbound sender");
    assert_eq!(valid.direct_routes[0].recipient(), 1180954);

    let sender_and_destination = CborValue::serialized(&serde_json::json!({
        "bot_email_secret":"email",
        "api_key_secret":"key",
        "identity_key_secret":"identity",
        "site":"https://chat.example.test",
        "allowed_user_ids":[42],
        "sender_aliases":[{"user_id":42, "alias":"dpc"}],
        "proactive_direct_messages":[{"alias":"dpc", "recipient":1180954}],
    }))
    .expect("config value")
    .deserialized::<ExtConfig>()
    .expect("config schema")
    .validate(&secrets);
    assert!(
        sender_and_destination.is_ok(),
        "presentation aliases do not occupy the destination namespace"
    );

    let Err(error) = parse(
        serde_json::json!([]),
        serde_json::json!([{"alias":"zero", "recipient":0}]),
    )
    .validate(&secrets) else {
        panic!("zero direct recipient accepted")
    };
    assert_eq!(
        error,
        "zulip proactive direct-message recipient must be a non-zero user ID"
    );

    let Err(error) = parse(
        serde_json::json!([{
            "name":"ops", "topic":"deploy", "proactive_send":true
        }]),
        serde_json::json!([{"alias":"ops", "recipient":1180954}]),
    )
    .validate(&secrets) else {
        panic!("stream/direct alias collision accepted")
    };
    assert_eq!(
        error,
        "zulip proactive direct-message aliases must be unique"
    );

    let routes = (0..64)
        .map(|index| {
            serde_json::json!({
                "name":format!("stream{index}"),
                "topic":"deploy",
                "proactive_send":true
            })
        })
        .collect::<Vec<_>>();
    let Err(error) = parse(
        serde_json::Value::Array(routes),
        serde_json::json!([{"alias":"dpc", "recipient":1180954}]),
    )
    .validate(&secrets) else {
        panic!("65 configured destinations accepted")
    };
    assert_eq!(
        error,
        "zulip config exceeds the 64-entry route or alias limit"
    );

    let Err(duplicate_alias) = parse(
        serde_json::json!([]),
        serde_json::json!([
            {"alias":"dpc", "recipient":1180954},
            {"alias":"dpc", "recipient":42}
        ]),
    )
    .validate(&secrets) else {
        panic!("duplicate direct alias accepted")
    };
    assert_eq!(
        duplicate_alias,
        "zulip proactive direct-message aliases must be unique"
    );

    let nested_unknown = CborValue::serialized(&serde_json::json!({
        "proactive_direct_messages":[{
            "alias":"dpc", "recipient":1180954, "unexpected":true
        }]
    }))
    .expect("config value");
    assert!(nested_unknown.deserialized::<ExtConfig>().is_err());
    let recipient_array = CborValue::serialized(&serde_json::json!({
        "proactive_direct_messages":[{"alias":"dpc", "recipients":[1180954]}]
    }))
    .expect("config value");
    assert!(recipient_array.deserialized::<ExtConfig>().is_err());
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

/// Mention-only stream ingress admits an exact allowlisted sender, preserves a
/// leading address, and emits no native authority.
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
    );
    let Event::MessageDeliveredReported(report) = event_from(rx.recv().expect("delivery report"))
    else {
        panic!("wrong report")
    };
    assert_eq!(report.text, "@**Tau Bot** restart service");
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

/// Admitted stream reports preserve leading and middle bot mentions plus
/// non-mentioned content so canonical publication never loses addressed
/// context.
#[test]
fn stream_ingress_preserves_verbatim_markdown_content() {
    let config = validated_config(serde_json::json!([{
        "name": "ops",
        "topic": "deploy",
        "receive": "all_messages"
    }]))
    .expect("valid all-messages route");
    let (ext, rx, _) = extension_with_config(config);
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);

    let cases = [
        (
            12,
            501,
            serde_json::json!(["mentioned"]),
            "@**Tau Bot** restart service",
        ),
        (
            13,
            502,
            serde_json::json!(["mentioned"]),
            "restart @**Tau Bot** service",
        ),
        (14, 503, serde_json::json!([]), "ordinary service message"),
    ];
    for (event_id, native_id, flags, expected_text) in cases {
        ext.process_event(
            serde_json::json!({
                "id": event_id, "type": "message", "flags": flags, "message": {
                    "id": native_id, "type": "stream", "sender_id": 42, "stream_id": 7,
                    "subject": "deploy", "content": expected_text
                }
            }),
            generation,
            registration,
            99,
        );
        let Event::MessageDeliveredReported(report) =
            event_from(rx.recv().expect("delivery report"))
        else {
            panic!("wrong report")
        };
        assert_eq!(report.text, expected_text);
    }
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

/// Direct-message admission requires complete, bounded, unique participant
/// evidence so malformed routes cannot omit a participant or narrow authority.
#[test]
fn direct_participant_admission_requires_complete_allowlisted_membership() {
    let mut config = cfg();
    config.allowed_user_ids.extend(1..=32);
    let maximum = (1..=31)
        .chain([42])
        .map(|id| serde_json::json!({"id":id}))
        .chain([serde_json::json!({"id":99})])
        .collect::<Vec<_>>();
    let mut one_over_maximum = maximum.clone();
    one_over_maximum.push(serde_json::json!({"id":32}));
    let cases = [
        ("missing", "private", None, None),
        ("null", "private", Some(serde_json::json!(null)), None),
        (
            "string",
            "private",
            Some(serde_json::json!("recipient")),
            None,
        ),
        (
            "object",
            "private",
            Some(serde_json::json!({"id":42})),
            None,
        ),
        ("empty", "private", Some(serde_json::json!([])), None),
        (
            "non-object member",
            "private",
            Some(serde_json::json!([42])),
            None,
        ),
        (
            "missing member ID",
            "private",
            Some(serde_json::json!([{"id":99}, {}])),
            None,
        ),
        (
            "null member ID",
            "private",
            Some(serde_json::json!([{"id":99}, {"id":null}])),
            None,
        ),
        (
            "string member ID",
            "private",
            Some(serde_json::json!([{"id":99}, {"id":"42"}])),
            None,
        ),
        (
            "negative member ID",
            "private",
            Some(serde_json::json!([{"id":99}, {"id":-42}])),
            None,
        ),
        (
            "zero member ID",
            "private",
            Some(serde_json::json!([{"id":99}, {"id":0}])),
            None,
        ),
        (
            "missing bot",
            "private",
            Some(serde_json::json!([{"id":42}])),
            None,
        ),
        (
            "missing sender",
            "private",
            Some(serde_json::json!([{"id":99}, {"id":7}])),
            None,
        ),
        (
            "duplicate",
            "private",
            Some(serde_json::json!([{"id":99}, {"id":42}, {"id":42}])),
            None,
        ),
        (
            "maximum raw participants",
            "private",
            Some(serde_json::Value::Array(maximum)),
            Some((1..=31).chain([42]).collect()),
        ),
        (
            "raw participant limit plus one",
            "private",
            Some(serde_json::Value::Array(one_over_maximum)),
            None,
        ),
        (
            "one-to-one",
            "direct",
            Some(serde_json::json!([{"id":99}, {"id":42}])),
            Some(vec![42]),
        ),
        (
            "allowed group",
            "private",
            Some(serde_json::json!([{"id":7}, {"id":99}, {"id":42}])),
            Some(vec![7, 42]),
        ),
        (
            "mixed allowlist",
            "private",
            Some(serde_json::json!([{"id":777}, {"id":99}, {"id":42}])),
            None,
        ),
    ];

    for (name, kind, recipients, expected_users) in cases {
        let mut message = serde_json::json!({"type":kind, "sender_id":42});
        if let Some(recipients) = recipients {
            message["display_recipient"] = recipients;
        }
        let users = admitted_conversation(&config, &message, 42, 99, false).map(|conversation| {
            let NativeRoute::Direct(users) = conversation.route else {
                panic!("{name}: expected direct route")
            };
            users
        });
        assert_eq!(users, expected_users, "{name}");
    }
}

/// Direct-message route identity sorts participants before applying the
/// existing hash domain, so equivalent provider orderings keep the same reply
/// route.
#[test]
fn direct_participant_routes_are_sorted_and_stably_hashed() {
    let mut config = cfg();
    config.allowed_user_ids.insert(7);
    let first = admitted_conversation(
        &config,
        &serde_json::json!({
            "type": "private", "sender_id": 42,
            "display_recipient": [{"id": 7}, {"id": 99}, {"id": 42}]
        }),
        42,
        99,
        false,
    )
    .expect("valid first participant ordering");
    let reordered = admitted_conversation(
        &config,
        &serde_json::json!({
            "type": "private", "sender_id": 42,
            "display_recipient": [{"id": 42}, {"id": 7}, {"id": 99}]
        }),
        42,
        99,
        false,
    )
    .expect("valid reordered participants");

    assert_eq!(first.route, NativeRoute::Direct(vec![7, 42]));
    assert_eq!(reordered.route, NativeRoute::Direct(vec![7, 42]));
    assert_eq!(
        first.stable_id,
        "zulip-direct:ab9d03690d0e16680d89d8d4302d6ff73d2a270a3e83d277b28df101ca96c530"
    );
    assert_eq!(reordered.stable_id, first.stable_id);
}

/// Rejected direct-message participant evidence emits no report and cannot
/// create the owner required for a later source-bound reply.
#[test]
fn malformed_direct_participants_create_no_report_or_reply_owner() {
    let (ext, rx, client) = extension();
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    ext.process_event(
        serde_json::json!({
            "id": 13, "type": "message", "message": {
                "id": 502, "type": "private", "sender_id": 42, "content": "incomplete",
                "display_recipient": [{"id": 42}]
            }
        }),
        generation,
        registration,
        99,
    );

    assert!(
        rx.try_recv().is_err(),
        "malformed event must not emit a report"
    );
    assert!(
        ext.state.lock().owners.is_empty(),
        "malformed event must not install an owner"
    );
    let result = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("reply".to_owned())),
            (
                "reply_to",
                CborValue::Text(message_fact_id(&cfg(), 502).as_str().to_owned()),
            ),
        ],
    ));
    assert!(matches!(result, Event::ToolError(_)));
    assert!(client.sends.lock().expect("sends").is_empty());
}

/// Configured aliases permit only their fixed direct recipient, retain
/// source-bound replies, and coexist with stream destinations without exposing
/// a recipient ID through discovery or tool arguments.
#[test]
fn proactive_direct_message_alias_is_fixed_and_coexists_with_other_routes() {
    let config = validated_config_with_direct_messages(
        serde_json::json!([{
            "name":"ops",
            "topic":"deploy",
            "receive":"mentions_only",
            "proactive_send":true,
            "description":"Operations"
        }]),
        serde_json::json!([{
            "alias":"dpc",
            "recipient":1180954,
            "description":"Operator escalation"
        }]),
    )
    .expect("valid direct and stream destinations");
    let source_config = config.clone();
    let (ext, _rx, client) = extension_with_config(config);
    let Event::ToolResult(discovery) =
        ext.handle_conversations(tool(CONVERSATIONS_TOOL_NAME, vec![]))
    else {
        panic!("expected discovery result")
    };
    let CborValue::Text(discovery) = discovery.result else {
        panic!("expected JSON discovery result")
    };
    let discovery: serde_json::Value =
        serde_json::from_str(&discovery).expect("valid discovery JSON");
    assert_eq!(
        discovery.pointer("/conversations/1/kind"),
        Some(&serde_json::Value::String("direct".to_owned()))
    );
    assert!(!discovery.to_string().contains("1180954"));

    let direct = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            (
                "message",
                CborValue::Text("proactive direct message".to_owned()),
            ),
            ("destination", CborValue::Text("dpc".to_owned())),
        ],
    ));
    assert!(matches!(direct, Event::ToolResult(_)));
    let stream = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            (
                "message",
                CborValue::Text("proactive stream message".to_owned()),
            ),
            ("destination", CborValue::Text("ops".to_owned())),
        ],
    ));
    assert!(matches!(stream, Event::ToolResult(_)));

    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    ext.process_event(
        serde_json::json!({
            "id":42, "type":"message", "message": {
                "id":502, "type":"private", "sender_id":42, "content":"reply to me", "flags":[],
                "display_recipient":[{"id":99},{"id":42}]
            }
        }),
        generation,
        registration,
        99,
    );
    let source_reply = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("source reply".to_owned())),
            (
                "reply_to",
                CborValue::Text(message_fact_id(&source_config, 502).as_str().to_owned()),
            ),
        ],
    ));
    assert!(matches!(source_reply, Event::ToolResult(_)));
    assert_eq!(
        client.sends.lock().expect("sends").as_slice(),
        &[
            (
                NativeRoute::Direct(vec![1180954]),
                "proactive direct message".to_owned(),
            ),
            (
                NativeRoute::Stream {
                    stream_id: 7,
                    topic: "deploy".to_owned(),
                },
                "proactive stream message".to_owned(),
            ),
            (NativeRoute::Direct(vec![42]), "source reply".to_owned()),
        ]
    );

    let numeric_destination = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("denied".to_owned())),
            ("destination", CborValue::Text("1180954".to_owned())),
        ],
    ));
    assert!(matches!(numeric_destination, Event::ToolError(_)));
    let direct_topic = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("denied".to_owned())),
            ("destination", CborValue::Text("dpc".to_owned())),
            ("topic", CborValue::Text("not-applicable".to_owned())),
        ],
    ));
    assert!(matches!(direct_topic, Event::ToolError(_)));
    let arbitrary_user_id = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("denied".to_owned())),
            ("destination", CborValue::Text("dpc".to_owned())),
            ("user_id", CborValue::Integer(1180954.into())),
        ],
    ));
    assert!(matches!(arbitrary_user_id, Event::ToolError(_)));
}

/// Sender presentation aliases never become outbound destinations, even when
/// their names differ from every configured stream and direct destination.
#[test]
fn sender_alias_cannot_select_an_outbound_destination() {
    let mut config = cfg();
    config.sender_aliases.insert(42, "inbound-only".to_owned());
    let (ext, _rx, client) = extension_with_config(config);
    let result = ext.handle_send(tool(
        SEND_TOOL_NAME,
        vec![
            ("message", CborValue::Text("denied".to_owned())),
            ("destination", CborValue::Text("inbound-only".to_owned())),
        ],
    ));
    assert!(matches!(result, Event::ToolError(_)));
    assert!(client.sends.lock().expect("sends").is_empty());
}

/// A configured direct send reports only an opaque conversation and the
/// configured alias, while the fake transport still receives its fixed native
/// recipient route.
#[test]
fn proactive_direct_send_keeps_recipient_private_in_report_and_result() {
    let config = validated_config_with_direct_messages(
        serde_json::json!([]),
        serde_json::json!([{"alias":"dpc", "recipient":1180954}]),
    )
    .expect("valid direct destination");
    let (ext, rx, client) = extension_with_config(config);
    ext.dispatch_tool(tool(
        SEND_TOOL_NAME,
        vec![
            (
                "message",
                CborValue::Text("private direct message".to_owned()),
            ),
            ("destination", CborValue::Text("dpc".to_owned())),
        ],
    ));
    assert!(matches!(
        event_from(rx.recv().expect("progress")),
        Event::ToolProgressReported(_)
    ));
    let Event::MessageSentReported(report) = event_from(rx.recv().expect("sent report")) else {
        panic!("missing direct sent report")
    };
    let conversation = report.conversation.as_ref().expect("direct conversation");
    assert!(conversation.stable_id.starts_with("zulip-direct:"));
    assert_eq!(conversation.alias.as_deref(), Some("dpc"));
    assert_eq!(report.recipient, None);
    let report_json = serde_json::to_string(&report).expect("serializable sent report");
    assert!(!report_json.contains("1180954"));
    assert!(!report_json.contains("recipient"));
    assert!(!report_json.contains("native"));
    let Event::ToolResultReported(result) = event_from(rx.recv().expect("result")) else {
        panic!("missing direct send result")
    };
    let CborValue::Text(result) = result.result else {
        panic!("text direct send result")
    };
    assert!(result.contains("zulip-message:"));
    assert!(!result.contains("1180954"));
    assert!(!result.contains("777"));
    assert_eq!(
        client.sends.lock().expect("sends").as_slice(),
        &[(
            NativeRoute::Direct(vec![1180954]),
            "private direct message".to_owned(),
        )]
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
    let route = config
        .configured_routes
        .first_mut()
        .expect("configured route");
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
    let route = config
        .configured_routes
        .first_mut()
        .expect("configured route");
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

/// A repeated native message ID must emit only its first report, preserving the
/// extension's process-local deduplication boundary.
#[test]
fn duplicate_native_message_is_suppressed() {
    let (ext, rx, _) = extension();
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    let event = serde_json::json!({"id":13,"type":"message","message":{"id":502,"type":"private","sender_id":42,"content":"once","flags":[],"display_recipient":[{"id":42},{"id":99}]}});
    ext.process_event(event.clone(), generation, registration, 99);
    ext.process_event(
        serde_json::json!({"id":14,"type":"message","message":{"id":502,"type":"private","sender_id":42,"content":"once","flags":[],"display_recipient":[{"id":42},{"id":99}]}}),
        generation,
        registration,
        99,
    );
    assert!(matches!(
        event_from(rx.recv().expect("one report")),
        Event::MessageDeliveredReported(_)
    ));
    assert!(rx.try_recv().is_err());
}

/// A fully admissible self-authored message must not report or install source
/// authority, preventing the bridge from feeding its own Zulip echo back in.
#[test]
fn self_authored_valid_message_is_suppressed() {
    let mut config = cfg();
    config.allowed_user_ids.insert(99);
    let (ext, rx, _) = extension_with_config(config);
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    ext.process_event(
        serde_json::json!({"id":14,"type":"message","message":{"id":503,"type":"private","sender_id":99,"content":"echo","flags":[],"display_recipient":[{"id":42},{"id":99}]}}),
        generation,
        registration,
        99,
    );
    assert!(rx.try_recv().is_err());
    assert!(
        !ext.state
            .lock()
            .owners
            .contains_key(message_fact_id(&cfg(), 503).as_str())
    );
    assert!(!ext.state.lock().recent_set.contains("message:503"));
}

/// A stale registration generation drops an otherwise admitted message before
/// it emits a report or mutates reply and duplicate-suppression ownership.
#[test]
fn stale_generation_drops_ingress() {
    let (ext, rx, _) = extension();
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    let current = serde_json::json!({"id":15,"type":"message","message":{"id":504,"type":"private","sender_id":42,"content":"current","flags":[],"display_recipient":[{"id":42},{"id":99}]}});
    ext.process_event(current, generation, registration, 99);
    assert!(matches!(
        event_from(rx.recv().expect("current-generation report")),
        Event::MessageDeliveredReported(_)
    ));
    ext.state.lock().registration_generation = ZulipRegistrationGeneration::new(2);
    ext.process_event(
        serde_json::json!({"id":16,"type":"message","message":{"id":505,"type":"private","sender_id":42,"content":"stale","flags":[],"display_recipient":[{"id":42},{"id":99}]}}),
        generation,
        registration,
        99,
    );
    assert!(rx.try_recv().is_err());
    let state = ext.state.lock();
    assert!(
        !state
            .owners
            .contains_key(message_fact_id(&cfg(), 505).as_str())
    );
    assert!(!state.recent_set.contains("message:505"));
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

/// Mutation reports preserve their opaque base correlation, untrusted payload,
/// and authenticated actor without exposing native Zulip routing; deletion then
/// revokes the source route.
#[test]
fn mutations_emit_immutable_reports_and_delete_revokes() {
    let (ext, rx, _) = extension();
    let state = ext.state.lock();
    let generation = state.config_generation;
    let registration = state.registration_generation;
    drop(state);
    ext.process_event(serde_json::json!({"id":20,"type":"message","message":{"id":600,"type":"private","sender_id":42,"content":"base","flags":[],"display_recipient":[{"id":42},{"id":99}]}}), generation, registration, 99);
    let _ = rx.recv().expect("base report");
    ext.process_event(serde_json::json!({"id":21,"type":"update_message","message_id":600,"user_id":42,"message":{"content":"**edited**\n\n[details](https://example.test)"}}), generation, registration, 99);
    ext.process_event(serde_json::json!({"id":22,"type":"reaction","message_id":600,"user_id":42,"op":"add","emoji_name":"thumbs_up"}), generation, registration, 99);
    ext.process_event(
        serde_json::json!({"id":23,"type":"delete_message","message_id":600,"user_id":42}),
        generation,
        registration,
        99,
    );
    let edit = event_from(rx.recv().expect("edit"));
    let reaction = event_from(rx.recv().expect("reaction"));
    let deletion = event_from(rx.recv().expect("delete"));
    let expected_fact_id = message_fact_id(&cfg(), 600);
    let expected_actor = sender_party(&cfg(), 42, Some("alice".to_owned()));

    let Event::MessageEditedReported(edit_report) = &edit else {
        panic!("expected edit report");
    };
    assert_eq!(edit_report.target.message_id, expected_fact_id);
    assert_eq!(
        edit_report.target.publisher_extension_id.as_str(),
        publisher().as_str()
    );
    assert_eq!(
        edit_report.text,
        "**edited**\n\n[details](https://example.test)"
    );
    assert_eq!(edit_report.actor.as_ref(), Some(&expected_actor));

    let Event::MessageReactionAddedReported(reaction_report) = &reaction else {
        panic!("expected added-reaction report");
    };
    assert_eq!(reaction_report.target.message_id, expected_fact_id);
    assert_eq!(
        reaction_report.target.publisher_extension_id.as_str(),
        publisher().as_str()
    );
    assert_eq!(reaction_report.reaction, "thumbs_up");
    assert_eq!(reaction_report.actor.as_ref(), Some(&expected_actor));

    let Event::MessageDeletedReported(delete_report) = &deletion else {
        panic!("expected delete report");
    };
    assert_eq!(delete_report.target.message_id, expected_fact_id);
    assert_eq!(
        delete_report.target.publisher_extension_id.as_str(),
        publisher().as_str()
    );
    assert_eq!(delete_report.actor.as_ref(), Some(&expected_actor));

    let serialized =
        serde_json::to_value(&[edit, reaction, deletion]).expect("serialize mutation reports");
    fn contains_native_detail(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Null | serde_json::Value::Bool(_) => false,
            serde_json::Value::Number(number) => {
                matches!(number.as_u64(), Some(42 | 99 | 600))
            }
            serde_json::Value::String(value) => matches!(
                value.as_str(),
                "42" | "99" | "600" | "display_recipient" | "stream_id" | "private"
            ),
            serde_json::Value::Array(values) => values.iter().any(contains_native_detail),
            serde_json::Value::Object(values) => values.iter().any(|(key, value)| {
                matches!(key.as_str(), "display_recipient" | "stream_id" | "private")
                    || contains_native_detail(value)
            }),
        }
    }
    assert!(
        !contains_native_detail(&serialized),
        "serialized report leaked native route detail"
    );
    assert!(
        !ext.state
            .lock()
            .owners
            .contains_key(message_fact_id(&cfg(), 600).as_str())
    );
}

/// Checked mutation publication retains the exact owner lock, preventing a
/// concurrent full-FIFO insert from evicting its source before publication.
#[test]
fn mutation_publication_blocks_owner_fifo_eviction() {
    let (ext, rx, _) = extension();
    let generation = ext.state.lock().config_generation;
    let registration = ext.state.lock().registration_generation;
    ext.process_event(serde_json::json!({"id":20,"type":"message","message":{"id":600,"type":"private","sender_id":42,"content":"base","flags":[],"display_recipient":[{"id":42},{"id":99}]}}), generation, registration, 99);
    let _ = rx.recv().expect("base report");
    {
        let mut state = ext.state.lock();
        let owner = state.owners.values().next().expect("base owner").clone();
        for index in 1..REPLY_ROUTE_LIMIT {
            let mut filler = owner.clone();
            filler.fact_id = MessageFactId::new(format!("zulip-message:filler-{index}"));
            filler.native_message_id = 600 + index as u64;
            state.insert_owner(filler);
        }
    }
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *MUTATION_PUBLICATION_HOOK
        .lock()
        .expect("mutation publication hook") = Some((entered_tx, release_rx));
    std::thread::scope(|scope| {
        let mutation = scope.spawn(|| {
            ext.process_event(serde_json::json!({"id":21,"type":"update_message","message_id":600,"user_id":42,"message":{"content":"edited"}}), generation, registration, 99);
        });
        entered_rx.recv().expect("mutation reached publication");
        assert!(ext.state.state.try_lock().is_err());
        release_tx.send(()).expect("release publication");
        mutation.join().expect("mutation");
    });
    assert!(matches!(
        event_from(rx.recv().expect("edit report")),
        Event::MessageEditedReported(_)
    ));
}

/// Create, edit, delete, and reaction events must leave the source cursor at
/// the preceding event when their mandatory report cannot be published.
#[test]
fn mandatory_report_failure_preserves_cursor_and_message_ownership() {
    let cases = [
        serde_json::json!({"id":21,"type":"update_message","message_id":600,"user_id":42,"message":{"content":"edited"}}),
        serde_json::json!({"id":21,"type":"delete_message","message_id":600,"user_id":42}),
        serde_json::json!({"id":21,"type":"reaction","message_id":600,"user_id":42,"op":"add","emoji_name":"thumbs_up"}),
        serde_json::json!({"id":21,"type":"reaction","message_id":600,"user_id":42,"op":"remove","emoji_name":"thumbs_up"}),
    ];
    for event in cases {
        let (ext, rx, _) = extension();
        let state = ext.state.lock();
        let generation = state.config_generation;
        let registration = state.registration_generation;
        drop(state);
        ext.process_event(serde_json::json!({"id":20,"type":"message","message":{"id":600,"type":"private","sender_id":42,"content":"base","flags":[],"display_recipient":[{"id":42},{"id":99}]}}), generation, registration, 99);
        let _ = rx.recv().expect("base report");
        drop(rx);

        assert_eq!(
            process_event_batch(&ext, vec![event], 20, generation, registration, 99),
            20
        );
        assert!(
            ext.state
                .lock()
                .owners
                .contains_key(message_fact_id(&cfg(), 600).as_str()),
            "failed mutation publication consumed message ownership"
        );
    }

    let (ext, rx, _) = extension();
    drop(rx);
    let generation = ext.state.lock().config_generation;
    assert_eq!(
        process_event_batch(
            &ext,
            vec![
                serde_json::json!({"id":21,"type":"message","message":{"id":601,"type":"private","sender_id":42,"content":"create","flags":[],"display_recipient":[{"id":42},{"id":99}]}})
            ],
            20,
            generation,
            ZulipRegistrationGeneration::new(1),
            99,
        ),
        20,
        "failed create publication advanced the source cursor"
    );
    assert!(
        !ext.state
            .lock()
            .owners
            .contains_key(message_fact_id(&cfg(), 601).as_str()),
        "failed create publication retained unusable reply ownership"
    );
}

/// A mixed event batch advances through published and safely skipped events,
/// stops at the first failed mandatory report, and leaves its suffix untouched.
#[test]
fn mixed_batch_advances_only_the_exact_published_prefix() {
    let (tx, rx) = mpsc::channel();
    let mut config = cfg();
    config.routes = config
        .configured_routes
        .iter()
        .map(|route| route.resolve(7))
        .collect();
    let output = Output::channel_failing_after(tx, 1);
    let ext = Extension::new(
        Arc::new(FakeClient::default()),
        output.clone(),
        ToolNames::logical(),
    );
    ext.apply_config(config, publisher());
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id());
        state.queue = Some(EventQueue {
            queue_id: "queue-secret".to_owned(),
            last_event_id: 0,
            bot_user_id: 99,
            poll_request_timeout: Duration::from_secs(100),
        });
        state.registration_generation = ZulipRegistrationGeneration::new(1);
    }
    let generation = ext.state.lock().config_generation;
    let registration = ext.state.lock().registration_generation;
    let first = serde_json::json!({"id":21,"type":"message","message":{"id":610,"type":"private","sender_id":42,"content":"published","flags":[],"display_recipient":[{"id":42},{"id":99}]}});
    let skipped = serde_json::json!({"id":22,"type":"heartbeat"});
    let failed = serde_json::json!({"id":23,"type":"message","message":{"id":611,"type":"private","sender_id":42,"content":"failed","flags":[],"display_recipient":[{"id":42},{"id":99}]}});
    let suffix = serde_json::json!({"id":24,"type":"message","message":{"id":612,"type":"private","sender_id":42,"content":"suffix","flags":[],"display_recipient":[{"id":42},{"id":99}]}});
    let cursor = process_event_batch(
        &ext,
        vec![first, skipped, failed, suffix],
        20,
        generation,
        registration,
        99,
    );
    assert_eq!(cursor, 22);
    assert_eq!(output.report_attempts(), 2);
    assert!(rx.try_recv().is_ok(), "prefix report was not published");
    let state = ext.state.lock();
    assert!(!state.recent_set.contains("message:612"));
}

/// A sole synchronous tool terminal must wait behind an exhausted production
/// detached FIFO and publish exactly once when writer admission resumes.
#[test]
fn tool_terminal_survives_production_fifo_saturation() {
    let _serial = SATURATION_TEST_LOCK
        .lock()
        .expect("zulip saturation test lock");
    let mut input_bytes = Vec::new();
    let mut input = tau_proto::HarnessOutputWriter::new(&mut input_bytes);
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
    input
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: publisher(),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_email_secret":"email",
                "api_key_secret":"key",
                "identity_key_secret":"identity",
                "site":"https://chat.example.test",
                "allowed_user_ids":[42],
                "conversations":[{
                    "name":"ops",
                    "topic":"deploy",
                    "receive":"mentions_only"
                }]
            })),
            state_dir: None,
            secrets,
            settings_files: Default::default(),
        }))
        .expect("configure");
    let mut invoke = tool(CONVERSATIONS_TOOL_NAME, vec![]);
    invoke.call_id = "gyf8-zulip-saturation-terminal".into();
    let call_id = invoke.call_id.clone();
    input
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(invoke)))
        .expect("invoke");
    input
        .write_message(&HarnessOutputMessage::Disconnect(Default::default()))
        .expect("disconnect");
    input.flush().expect("flush input");

    let bytes = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new((Mutex::new(true), Condvar::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (saturated_tx, saturated_rx) = mpsc::channel();
    *SATURATION_HOOK.lock().expect("zulip saturation hook") =
        Some((call_id.as_str().to_owned(), saturated_tx));
    let hook = SaturationHookGuard;
    let output_bytes = Arc::clone(&bytes);
    let output_gate = Arc::clone(&gate);
    let runner = std::thread::spawn(move || {
        run_with_client(
            Cursor::new(input_bytes),
            SaturationWriter {
                bytes: output_bytes,
                gate: output_gate,
                entered: entered_tx,
                blocked: false,
            },
            Arc::new(FakeClient::default()),
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

    let mut reader =
        tau_proto::HarnessInputReader::new(Cursor::new(bytes.lock().expect("bytes").clone()));
    let terminals = std::iter::from_fn(|| reader.read_message().transpose())
        .collect::<Result<Vec<_>, _>>()
        .expect("decode output")
        .into_iter()
        .filter(|frame| {
            matches!(
                frame,
                HarnessInputMessage::Emit(emit)
                    if matches!(emit.event.as_ref(),
                        Event::ToolResultReported(result) if result.call_id == call_id
                    ) || matches!(emit.event.as_ref(),
                        Event::ToolErrorReported(error) if error.call_id == call_id
                    )
            )
        })
        .count();
    assert_eq!(terminals, 1);
}

/// Failure of a checked sole terminal must return from the production runner
/// instead of leaving the configured extension connected without settlement.
#[test]
fn tool_terminal_writer_failure_exits_production_loop() {
    let mut input_bytes = Vec::new();
    let mut input = tau_proto::HarnessOutputWriter::new(&mut input_bytes);
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
    input
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: publisher(),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_email_secret":"email",
                "api_key_secret":"key",
                "identity_key_secret":"identity",
                "site":"https://chat.example.test",
                "allowed_user_ids":[42],
                "conversations":[{
                    "name":"ops",
                    "topic":"deploy",
                    "receive":"mentions_only"
                }]
            })),
            state_dir: None,
            secrets,
            settings_files: Default::default(),
        }))
        .expect("configure");
    let mut invoke = tool(CONVERSATIONS_TOOL_NAME, vec![]);
    invoke.call_id = "gyf8-zulip-failed-terminal".into();
    input
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(invoke)))
        .expect("invoke");
    input.flush().expect("flush input");

    let (failed_tx, failed_rx) = mpsc::channel();
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let result = run_with_client(
        Cursor::new(input_bytes),
        FailingWriter {
            bytes: Arc::clone(&bytes),
            target: b"gyf8-zulip-failed-terminal",
            failed: Some(failed_tx),
            skip_matches: 1,
        },
        Arc::new(FakeClient::default()),
    );
    assert!(
        result.is_err(),
        "terminal writer failure must exit the loop"
    );
    failed_rx
        .try_recv()
        .expect("selected writer failure occurred");
}

/// A queue-worker report failure must wake an otherwise idle protocol loop and
/// make disconnect cleanup retire its retained authority.
#[test]
fn ingress_report_writer_failure_wakes_idle_production_loop() {
    let (extension_input, harness_input) = UnixStream::pair().expect("input pair");
    let client = Arc::new(FakeClient::default());
    *client.live_events.lock().expect("live events lock") = Some(vec![serde_json::json!({
        "id": 11, "type": "message", "flags": ["mentioned"], "message": {
            "id": 501, "sender_id": 42, "content": "wake idle loop",
            "type": "stream", "stream_id": 7, "subject": "deploy"
        }
    })]);
    let runner_client: Arc<dyn ZulipClient> = client.clone();
    let (failed_tx, failed_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run_with_client(
            extension_input,
            FailingWriter {
                bytes: Arc::new(Mutex::new(Vec::new())),
                target: b"message.delivered_reported",
                failed: Some(failed_tx),
                skip_matches: 0,
            },
            runner_client,
        )
        .map_err(|error| error.to_string());
        let _ = result_tx.send(result);
    });

    let mut input = tau_proto::HarnessOutputWriter::new(harness_input);
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
    input
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: publisher(),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_email_secret":"email",
                "api_key_secret":"key",
                "identity_key_secret":"identity",
                "site":"https://chat.example.test",
                "allowed_user_ids":[42],
                "conversations":[{
                    "name":"ops",
                    "topic":"deploy",
                    "receive":"mentions_only"
                }]
            })),
            state_dir: None,
            secrets,
            settings_files: Default::default(),
        }))
        .expect("configure");
    input
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            vec![("enabled", CborValue::Bool(true))],
        ))))
        .expect("register");
    input.flush().expect("flush startup");

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
    assert_eq!(
        client.active_event_polls.load(AtomicOrdering::SeqCst),
        0,
        "queue worker remained inside a poll after runner cleanup"
    );
    let guard = client
        .worker_exit_lock
        .lock()
        .expect("worker exit wait lock");
    let _ = client
        .worker_exit_changed
        .wait_timeout_while(guard, Duration::from_secs(2), |_| {
            client.worker_exits.load(AtomicOrdering::SeqCst) == 0
        })
        .expect("worker exit wait");
    assert_eq!(client.worker_exits.load(AtomicOrdering::SeqCst), 1);
}

/// The cleanup path used after worker output failure clears all process-local
/// routing authority before the runner returns.
#[test]
fn output_failure_cleanup_clears_routing_authority() {
    let (ext, _rx, _) = extension();
    let generation = ext.state.lock().registration_generation;
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id());
        state.queue = Some(EventQueue {
            queue_id: "queue".to_owned(),
            last_event_id: 1,
            bot_user_id: 99,
            poll_request_timeout: Duration::from_secs(1),
        });
        state.insert_recent("message:7".to_owned());
        state.insert_owner(MessageOwner {
            agent_id: agent_id(),
            fact_id: MessageFactId::new("zulip-message:test"),
            native_message_id: 7,
            conversation: Conversation {
                route: NativeRoute::Direct(vec![42]),
                stable_id: "zulip-conversation:test".to_owned(),
                alias: None,
            },
        });
    }
    ext.request_shutdown();
    let state = ext.state.lock();
    assert!(state.registered_agents.is_empty());
    assert!(state.queue.is_none());
    assert!(state.owners.is_empty());
    assert!(state.owner_order.is_empty());
    assert!(state.recent_ids.is_empty());
    assert!(state.recent_set.is_empty());
    assert!(state.checkpoint.is_none());
    assert_eq!(state.registration_generation, generation.wrapping_next());
}

/// Authority retirement waits for the dedicated publication gate rather than
/// racing a canonical report under the mutable state mutex.
#[test]
fn reconfigure_waits_for_in_flight_publication_authority() {
    let (ext, _rx, _) = extension();
    let generation = ext.state.lock().config_generation;
    let publication = ext.publication_authority.publish();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (completed_tx, completed_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let reconfigure = scope.spawn(|| {
            entered_tx.send(()).expect("entered reconfigure");
            ext.apply_config(cfg(), publisher());
            completed_tx.send(()).expect("completed reconfigure");
        });
        entered_rx.recv().expect("reconfigure entered");
        assert!(completed_rx.try_recv().is_err());
        assert_eq!(ext.state.lock().config_generation, generation);
        drop(publication);
        completed_rx.recv().expect("reconfigure completed");
        reconfigure.join().expect("reconfigure");
    });
    assert_eq!(
        ext.state.lock().config_generation,
        generation.wrapping_next()
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

/// History requests must use Zulip's anchor pagination shape rather than an
/// unsupported comparison narrow, and must honor the server completion marker.
#[test]
fn http_history_uses_anchor_pagination_without_id_narrow() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Zulip");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept history request");
        let request = read_http_request(&mut socket);
        let request_line = request.lines().next().expect("request line");
        assert!(request_line.starts_with("GET /api/v1/messages?"));
        let query = request_line
            .split_whitespace()
            .nth(1)
            .and_then(|target| target.split_once('?'))
            .map(|(_, query)| query)
            .expect("query");
        let fields: BTreeMap<_, _> = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(fields.get("anchor").map(String::as_str), Some("41"));
        assert_eq!(fields.get("num_before").map(String::as_str), Some("0"));
        assert_eq!(fields.get("num_after").map(String::as_str), Some("100"));
        assert_eq!(
            fields.get("include_anchor").map(String::as_str),
            Some("false")
        );
        assert!(!fields.contains_key("narrow"));
        let body = r#"{"result":"success","messages":[{"id":42}],"found_newest":true}"#;
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("history response");
    });
    let mut config = cfg();
    config.api_base = format!("http://{address}/api/v1");
    let page = HttpZulipClient::default()
        .get_messages_after(&config, 41, 100)
        .expect("history page");
    assert!(page.found_newest);
    assert_eq!(page.messages[0]["id"], 42);
    server.join().expect("server");
}

/// Name-only channel configuration must resolve a private native ID and submit
/// the same configured name for the all-message subscription before queue
/// setup.
#[test]
fn http_resolves_and_subscribes_named_channel() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Zulip");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut resolve_socket, _) = listener.accept().expect("accept stream resolution");
        let resolve_request = read_http_request(&mut resolve_socket);
        let request_line = resolve_request.lines().next().expect("request line");
        assert!(request_line.starts_with("GET /api/v1/get_stream_id?"));
        let query = request_line
            .split_whitespace()
            .nth(1)
            .and_then(|target| target.split_once('?'))
            .map(|(_, query)| query)
            .expect("query");
        let query: BTreeMap<_, _> = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(query.get("stream").map(String::as_str), Some("Engineering"));
        let resolve_body = r#"{"result":"success","stream_id":7}"#;
        write!(
            resolve_socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{resolve_body}",
            resolve_body.len()
        )
        .expect("respond with stream ID");

        let (mut subscribe_socket, _) = listener.accept().expect("accept subscription");
        let subscribe_request = read_http_request(&mut subscribe_socket);
        assert!(subscribe_request.starts_with("POST /api/v1/users/me/subscriptions "));
        let (_, form) = subscribe_request
            .split_once("\r\n\r\n")
            .expect("subscription form");
        let form: BTreeMap<_, _> = url::form_urlencoded::parse(form.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(
            form.get("subscriptions")
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok()),
            Some(serde_json::json!([{"name":"Engineering"}]))
        );
        assert_eq!(
            form.get("authorization_errors_fatal").map(String::as_str),
            Some("true")
        );
        let subscribe_body = r#"{"result":"success"}"#;
        write!(
            subscribe_socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{subscribe_body}",
            subscribe_body.len()
        )
        .expect("respond with subscription");
    });
    let mut config = cfg();
    config.api_base = format!("http://{address}/api/v1");
    let client = HttpZulipClient::default();

    assert_eq!(
        client
            .resolve_stream_id(&config, "Engineering")
            .expect("stream ID"),
        7
    );
    client
        .subscribe(&config, &["Engineering".to_owned()])
        .expect("subscription");
    server.join().expect("server");
}

/// The production client must reject a provider page larger than the requested
/// bound even when the provider marks it terminal.
#[test]
fn http_history_rejects_oversized_provider_page() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Zulip");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept history request");
        let _request = read_http_request(&mut socket);
        let body = serde_json::json!({
            "result": "success",
            "messages": (1..=101).map(|id| serde_json::json!({"id": id})).collect::<Vec<_>>(),
            "found_newest": true
        })
        .to_string();
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("history response");
    });
    let mut config = cfg();
    config.api_base = format!("http://{address}/api/v1");
    assert!(matches!(
        HttpZulipClient::default().get_messages_after(&config, 0, 100),
        Err(ApiError::MalformedResponse)
    ));
    server.join().expect("server");
}

fn newest_history_result(
    messages: serde_json::Value,
    found_newest: bool,
) -> Result<Option<u64>, ApiError> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Zulip");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept newest request");
        let request = read_http_request(&mut socket);
        assert!(request.contains("anchor=newest"));
        assert!(request.contains("include_anchor=true"));
        let body = serde_json::json!({
            "result": "success",
            "messages": messages,
            "found_newest": found_newest
        })
        .to_string();
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("newest response");
    });
    let mut config = cfg();
    config.api_base = format!("http://{address}/api/v1");
    let result = HttpZulipClient::default().newest_message_id(&config);
    server.join().expect("server");
    result
}

/// First-use baselining must fail closed on malformed, non-increasing, or
/// nonterminal newest pages; only a valid terminal empty page means no
/// messages.
#[test]
fn newest_history_validation_cannot_fall_back_to_zero() {
    assert!(matches!(
        newest_history_result(serde_json::json!([{"id": "bad"}]), true),
        Err(ApiError::MalformedResponse)
    ));
    assert!(matches!(
        newest_history_result(serde_json::json!([{"id": 2}, {"id": 1}]), true),
        Err(ApiError::MalformedResponse)
    ));
    assert!(matches!(
        newest_history_result(serde_json::json!([{"id": 2}]), false),
        Err(ApiError::MalformedResponse)
    ));
    assert_eq!(
        newest_history_result(serde_json::json!([]), true).expect("valid empty newest page"),
        None
    );
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
            form.get("all_public_streams").map(String::as_str),
            Some("false"),
            "queue registration must not broaden stream visibility"
        );
        assert_eq!(
            form.get("event_types")
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok()),
            Some(serde_json::json!([
                "message",
                "update_message",
                "delete_message",
                "reaction"
            ])),
            "registration must retain explicit Zulip mutation event subscriptions"
        );
        assert_eq!(
            form.get("client_capabilities")
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok()),
            Some(serde_json::json!({
                "notification_settings_null": false,
                "empty_topic_name": true,
            })),
            "registration must include Zulip's historical required capability \
             while preserving its canonical empty general-chat topic"
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
        Some(ZulipRegistrationGeneration::new(1)),
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
