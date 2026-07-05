use std::collections::VecDeque;
use std::io::{Read as _, Write as _};

use super::*;

/// Ensures the daemon refuses to start without an explicit user allowlist.
#[test]
fn gateway_args_require_allowed_user() {
    let args = ["tau-telegram-gateway"];
    let result = GatewayConfig::from_env_args(args.map(OsString::from), |name| {
        (name == DEFAULT_BOT_TOKEN_ENV).then(|| "token".to_owned())
    });

    match result {
        Ok(_) => panic!("missing allowlist should fail"),
        Err(error) => assert!(error.contains("allowed")),
    }
}

/// Ensures command-line parsing resolves token, allowlist, and private
/// paths without ever taking the token from argv.
#[test]
fn gateway_args_resolve_token_from_environment() {
    let args = [
        "tau-telegram-gateway",
        "--bot-token-env",
        "BOT",
        "--allowed-user-id",
        "123",
        "--chat-id",
        "456",
        "--state-dir",
        "/tmp/tau-state",
        "--runtime-dir",
        "/tmp/tau-run",
    ];
    let config = GatewayConfig::from_env_args(args.map(OsString::from), |name| {
        (name == "BOT").then(|| "secret-token".to_owned())
    })
    .expect("gateway config should parse");

    assert_eq!(config.bot_token, "secret-token");
    assert!(config.allowed_user_ids.contains(&123));
    assert_eq!(config.configured_chat_id, Some(456));
    assert_eq!(config.state_dir, PathBuf::from("/tmp/tau-state"));
    assert_eq!(config.runtime_dir, PathBuf::from("/tmp/tau-run"));
}

/// Ensures durable state survives save/load and remains scoped to its
/// stream fingerprint.
#[test]
fn durable_state_round_trips_with_recent_updates() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("state.json");
    let mut state = GatewayDurableState {
        stream_hash: "stream".to_owned(),
        next_update_offset: Some(43),
        linked_chat: Some(GatewayLinkedChat {
            chat_id: 10,
            user_id: 20,
        }),
        recent_update_ids: Vec::new(),
        processed_update_count: 1,
        rejected_update_count: 0,
    };
    state.remember_update(42);
    state.save(&path).expect("save durable state");

    let loaded = GatewayDurableState::load(&path, "stream").expect("load durable state");

    assert_eq!(loaded.next_update_offset, Some(43));
    assert!(loaded.has_recent_update(42));
    assert_eq!(loaded.linked_chat.expect("link").chat_id, 10);
}

/// Ensures the handler rejects unallowlisted Telegram users before any
/// reply side effect, while still advancing the durable offset.
#[test]
fn unallowlisted_update_is_ignored_without_reply() {
    let mut fixture = GatewayFixture::new(None, [1]);
    fixture
        .gateway
        .process_update(update(7, message(99, 99, "/status")))
        .expect("process update");

    assert_eq!(fixture.gateway.durable.next_update_offset, Some(8));
    assert_eq!(fixture.client.sent.lock().expect("sent lock").len(), 0);
    assert_eq!(fixture.gateway.durable.rejected_update_count, 1);
}

/// Ensures /start links one allowlisted private chat and /status replies
/// through the same chat before full routing is implemented.
#[test]
fn start_links_private_chat_and_status_replies() {
    let mut fixture = GatewayFixture::new(None, [7]);
    fixture
        .gateway
        .process_update(update(1, message(7, 70, "/start")))
        .expect("process start");
    fixture
        .gateway
        .process_update(update(2, message(7, 70, "/status")))
        .expect("process status");

    assert_eq!(
        fixture.gateway.durable.linked_chat,
        Some(GatewayLinkedChat {
            chat_id: 70,
            user_id: 7
        })
    );
    let sent = fixture.client.sent.lock().expect("sent lock");
    assert_eq!(sent.len(), 2);
    assert!(sent[0].1.contains("MVP is running"));
    assert!(sent[1].1.contains("Tau Telegram gateway status"));
}

/// Ensures duplicate update ids are not handled twice after being
/// remembered in durable state.
#[test]
fn duplicate_update_id_is_not_reprocessed() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    let update = update(5, message(7, 10, "/status"));
    fixture
        .gateway
        .process_update(update.clone())
        .expect("process first update");
    fixture
        .gateway
        .process_update(update)
        .expect("process duplicate update");

    let sent = fixture.client.sent.lock().expect("sent lock");
    assert_eq!(sent.len(), 1);
    assert_eq!(fixture.gateway.durable.next_update_offset, Some(6));
}

/// Ensures an unconfigured group cannot cause gateway replies or chat
/// linking side effects, even when the sender is allowlisted.
#[test]
fn unconfigured_group_start_is_ignored_without_reply() {
    let mut fixture = GatewayFixture::new(None, [7]);
    let mut group_message = message(7, -100, "/start");
    group_message.chat_type = Some("supergroup".to_owned());

    fixture
        .gateway
        .process_update(update(9, group_message))
        .expect("process group update");

    assert_eq!(fixture.gateway.durable.linked_chat, None);
    assert_eq!(fixture.client.sent.lock().expect("sent lock").len(), 0);
}

/// Ensures a required `/status` reply failure leaves the offset unadvanced
/// so Telegram may redeliver instead of being silently skipped.
#[test]
fn status_send_failure_does_not_advance_offset() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.client.fail_next_sends(1);

    let outcome = fixture
        .gateway
        .process_update(update(11, message(7, 10, "/status")))
        .expect("process failed status send");

    assert_eq!(outcome, UpdateOutcome::NeedsRedelivery);
    assert_eq!(fixture.gateway.durable.next_update_offset, None);
    assert!(!fixture.gateway.durable.has_recent_update(11));
}

/// Ensures a failed reply-dependent update stops processing later updates in
/// the same Telegram batch so a later success cannot skip the failed update.
#[test]
fn failed_reply_stops_batch_before_later_update_advances_offset() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.client.fail_next_sends(1);

    fixture
        .gateway
        .process_updates(vec![
            update(11, message(7, 10, "/status")),
            update(12, message(99, 10, "/status")),
        ])
        .expect("process update batch");

    assert_eq!(fixture.gateway.durable.next_update_offset, None);
    assert!(!fixture.gateway.durable.has_recent_update(11));
    assert!(!fixture.gateway.durable.has_recent_update(12));
    assert_eq!(fixture.gateway.durable.rejected_update_count, 0);
}

/// Ensures first `/start` only commits the durable private-chat link after
/// the confirmation/help reply succeeds.
#[test]
fn start_send_failure_does_not_commit_link_or_offset() {
    let mut fixture = GatewayFixture::new(None, [7]);
    fixture.client.fail_next_sends(1);

    fixture
        .gateway
        .process_update(update(12, message(7, 70, "/start")))
        .expect("process failed start send");

    assert_eq!(fixture.gateway.durable.linked_chat, None);
    assert_eq!(fixture.gateway.durable.next_update_offset, None);
    assert!(!fixture.gateway.durable.has_recent_update(12));
}

/// Ensures an allowlisted user in a different unconfigured group cannot
/// trigger even a fixed-chat rejection reply.
#[test]
fn configured_chat_ignores_other_group_without_reply() {
    let mut fixture = GatewayFixture::new(Some(-200), [7]);
    let mut group_message = message(7, -100, "/start");
    group_message.chat_type = Some("supergroup".to_owned());

    fixture
        .gateway
        .process_update(update(13, group_message))
        .expect("process group update");

    assert_eq!(fixture.client.sent.lock().expect("sent lock").len(), 0);
    assert_eq!(fixture.gateway.durable.next_update_offset, Some(14));
}

/// Ensures a durable link owned by a removed allowlisted user is reconciled
/// away so a newly allowlisted private user can link.
#[test]
fn stale_link_from_removed_user_is_reconciled() {
    let cfg = runtime_config(None, [2]);
    let mut durable = GatewayDurableState {
        stream_hash: "test-stream".to_owned(),
        linked_chat: Some(GatewayLinkedChat {
            chat_id: 10,
            user_id: 1,
        }),
        ..GatewayDurableState::default()
    };

    assert!(durable.reconcile_with_config(&cfg));
    assert_eq!(durable.linked_chat, None);

    let mut fixture = GatewayFixture::with_durable(cfg, durable);
    fixture
        .gateway
        .process_update(update(14, message(2, 20, "/start")))
        .expect("process relink start");

    assert_eq!(
        fixture.gateway.durable.linked_chat,
        Some(GatewayLinkedChat {
            chat_id: 20,
            user_id: 2
        })
    );
}

/// Ensures fixed-chat mode clears stale private links instead of preserving
/// them for later resurrection.
#[test]
fn fixed_chat_reconciles_stale_private_link() {
    let cfg = runtime_config(Some(99), [7]);
    let mut durable = GatewayDurableState {
        stream_hash: "test-stream".to_owned(),
        linked_chat: Some(GatewayLinkedChat {
            chat_id: 70,
            user_id: 7,
        }),
        ..GatewayDurableState::default()
    };

    assert!(durable.reconcile_with_config(&cfg));

    assert_eq!(durable.linked_chat, None);
}

/// Ensures the local socket request parser accepts the versioned status
/// request shape intended for gateway-client discovery.
#[test]
fn local_socket_accepts_status_request() {
    let (mut client, server) = UnixStream::pair().expect("socket pair");
    writeln!(client, r#"{{"protocol_version":1,"kind":"status"}}"#).expect("write request");

    read_gateway_socket_request(&server).expect("status request should parse");
}

/// Ensures one local socket client cannot force unbounded request
/// buffering.
#[test]
fn local_socket_rejects_oversized_request() {
    let (mut client, server) = UnixStream::pair().expect("socket pair");
    client
        .write_all(&vec![b'a'; MAX_SOCKET_REQUEST_BYTES + 1])
        .expect("write oversized request");

    let error = read_gateway_socket_request(&server).expect_err("request should be too large");

    assert!(error.contains("too large"));
}

/// Ensures the local status socket response path returns valid JSON with
/// the core fields gateway clients need for discovery.
#[test]
fn local_socket_status_response_contains_core_fields() {
    let cfg = runtime_config(Some(10), [7]);
    let durable = GatewayDurableState {
        stream_hash: "test-stream".to_owned(),
        next_update_offset: Some(42),
        ..GatewayDurableState::default()
    };
    let status = Arc::new(Mutex::new(GatewayStatus::new(
        &cfg,
        &durable,
        "test-stream".to_owned(),
        PathBuf::from("/tmp/test.sock"),
    )));
    let (mut client, server) = UnixStream::pair().expect("socket pair");
    writeln!(client, r#"{{"protocol_version":1,"kind":"status"}}"#).expect("write request");

    handle_gateway_socket_client(server, status);

    let mut response = String::new();
    client
        .read_to_string(&mut response)
        .expect("read status response");
    let value: serde_json::Value =
        serde_json::from_str(&response).expect("response should be JSON");
    assert_eq!(value["protocol_version"], SOCKET_PROTOCOL_VERSION);
    assert_eq!(value["stream_hash"], "test-stream");
    assert_eq!(value["next_update_offset"], 42);
    assert_eq!(value["routing"], "help/status-only");
}

/// Fake Telegram client state captured by gateway tests.
#[derive(Default)]
struct FakeGatewayClient {
    /// Queued update batches returned by polling.
    updates: Mutex<VecDeque<Vec<TgUpdate>>>,
    /// Telegram replies sent by the gateway.
    sent: Mutex<Vec<(i64, String)>>,
    /// Number of future sends that should fail.
    send_failures: Mutex<usize>,
}

impl FakeGatewayClient {
    /// Cause the next `count` sends to fail.
    fn fail_next_sends(&self, count: usize) {
        *self.send_failures.lock().expect("failures lock") = count;
    }
}

impl TelegramClient for FakeGatewayClient {
    fn get_webhook_info(&self, _cfg: &RuntimeConfig) -> Result<crate::TgWebhookInfo, String> {
        Ok(crate::TgWebhookInfo::default())
    }

    fn get_updates(
        &self,
        _cfg: &RuntimeConfig,
        _offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, String> {
        Ok(self
            .updates
            .lock()
            .expect("updates lock")
            .pop_front()
            .unwrap_or_default())
    }

    fn send_message(&self, _cfg: &RuntimeConfig, chat_id: i64, text: &str) -> Result<(), String> {
        let mut failures = self.send_failures.lock().expect("failures lock");
        if *failures > 0 {
            *failures -= 1;
            return Err("simulated send failure".to_owned());
        }
        drop(failures);
        self.sent
            .lock()
            .expect("sent lock")
            .push((chat_id, text.to_owned()));
        Ok(())
    }
}

/// Test fixture containing a gateway with lock/socket filesystem effects
/// avoided so unit tests can focus on routing state.
struct GatewayFixture {
    /// Gateway under test.
    gateway: Gateway,
    /// Shared fake Telegram client.
    client: Arc<FakeGatewayClient>,
    /// Temporary directory keeping the test state path alive.
    _tempdir: tempfile::TempDir,
}

impl GatewayFixture {
    /// Create a gateway fixture with an optional configured chat.
    fn new<const N: usize>(configured_chat_id: Option<i64>, allowed_user_ids: [i64; N]) -> Self {
        let cfg = runtime_config(configured_chat_id, allowed_user_ids);
        let durable = GatewayDurableState {
            stream_hash: "test-stream".to_owned(),
            ..GatewayDurableState::default()
        };
        Self::with_durable(cfg, durable)
    }

    /// Create a gateway fixture with an explicit runtime config and durable
    /// state.
    fn with_durable(cfg: RuntimeConfig, durable: GatewayDurableState) -> Self {
        let client = Arc::new(FakeGatewayClient::default());
        let gateway_client: Arc<dyn TelegramClient> = client.clone();
        let tempdir = tempfile::tempdir().expect("tempdir");
        let status = Arc::new(Mutex::new(GatewayStatus::new(
            &cfg,
            &durable,
            "test-stream".to_owned(),
            PathBuf::from("/tmp/test.sock"),
        )));
        Self {
            gateway: Gateway {
                cfg,
                client: gateway_client,
                state_path: tempdir.path().join("state.json"),
                durable,
                status,
                _resources: GatewayResources::Test,
            },
            client,
            _tempdir: tempdir,
        }
    }
}

/// Build a runtime config for tests.
fn runtime_config<const N: usize>(
    configured_chat_id: Option<i64>,
    allowed_user_ids: [i64; N],
) -> RuntimeConfig {
    RuntimeConfig {
        bot_token: "token".to_owned(),
        allowed_user_ids: allowed_user_ids.into_iter().collect(),
        configured_chat_id,
        api_base: DEFAULT_API_BASE.to_owned(),
        poll_timeout_seconds: DEFAULT_POLL_TIMEOUT_SECONDS,
    }
}

/// Build a Telegram update for tests.
fn update(update_id: i64, message: TgMessage) -> TgUpdate {
    TgUpdate {
        update_id,
        message: Some(message),
    }
}

/// Build a private text message for tests.
fn message(user_id: i64, chat_id: i64, text: &str) -> TgMessage {
    TgMessage {
        chat_id,
        chat_type: Some("private".to_owned()),
        user_id,
        from_name: Some("tester".to_owned()),
        text: Some(text.to_owned()),
    }
}
