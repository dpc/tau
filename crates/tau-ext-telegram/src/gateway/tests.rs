use std::collections::VecDeque;
use std::io as path_std_io;
use std::io::BufRead as _;
use std::sync::Barrier;

use super::*;
use crate::gateway_client::{GatewayClient, GatewayClientConfig, GatewaySocketResponse};

/// Ensures the daemon refuses to start without an explicit user allowlist.
#[test]
fn gateway_args_require_allowed_user() {
    let args = ["tau-telegram-gateway"];
    let result = GatewayConfig::from_env_args(args.map(OsString::from), |name| {
        (name == DEFAULT_BOT_TOKEN_ENV).then(|| "token".to_owned())
    });

    match result {
        Ok(_) => panic!("missing allowlist should fail"),
        Err(error) => assert!(error.to_string().contains("allowed")),
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
        selected_route: None,
        checkpoints: GatewayCheckpoints::default(),
        recent_acknowledgements: VecDeque::new(),
    };
    state.remember_update(42);
    state.save(&path).expect("save durable state");

    let loaded = GatewayDurableState::load(&path, "stream").expect("load durable state");

    assert_eq!(loaded.next_update_offset, Some(43));
    assert!(loaded.has_recent_update(42));
    assert_eq!(loaded.linked_chat.expect("link").chat_id, 10);
}

/// Ensures committed ACK retry authorization stays oldest-first, bounded, and
/// content-free inside the existing gateway state schema.
#[test]
fn recent_acknowledgements_are_bounded_and_content_free() {
    let mut state = GatewayDurableState {
        stream_hash: "stream".to_owned(),
        ..GatewayDurableState::default()
    };
    let route = GatewayRegistrationKey {
        session_id: "session-alpha".to_owned(),
        agent_id: "agent-alpha".to_owned(),
    };
    for update_id in 0..=RECENT_ACKNOWLEDGEMENT_LIMIT {
        state.remember_acknowledgement(
            TelegramReportId::for_gateway(
                "stream",
                TelegramUpdateId::new(update_id as i64).expect("valid update"),
            ),
            route.clone(),
        );
    }

    assert_eq!(
        state.recent_acknowledgements.len(),
        RECENT_ACKNOWLEDGEMENT_LIMIT
    );
    assert_eq!(
        state
            .recent_acknowledgements
            .front()
            .expect("oldest retained ACK")
            .report_id,
        TelegramReportId::for_gateway("stream", TelegramUpdateId::new(1).expect("valid update"))
    );
    let encoded = serde_json::to_string(&state).expect("encode durable state");
    assert!(!encoded.contains("\"text\""));
    assert!(!encoded.contains("\"message_id\""));
}

/// Ensures corrupt persisted retry authorization cannot construct an untyped
/// report identity during gateway restart.
#[test]
fn durable_state_rejects_invalid_recent_ack_report_id() {
    let json = r#"{
        "stream_hash":"stream",
        "recent_acknowledgements":[{
            "report_id":"not-a-gateway-report",
            "route":{"session_id":"session","agent_id":"agent"}
        }]
    }"#;

    assert!(serde_json::from_str::<GatewayDurableState>(json).is_err());
}

/// Ensures restart rejects malformed, wrong-domain, and wrong-derived routed
/// report identities instead of replaying an unacknowledgeable checkpoint.
#[test]
fn durable_state_rejects_invalid_checkpoint_report_ids() {
    let update_id = TelegramUpdateId::new(42).expect("valid update");
    let expected = TelegramReportId::for_gateway("stream", update_id);
    let mut state = GatewayDurableState {
        stream_hash: "stream".to_owned(),
        ..GatewayDurableState::default()
    };
    state.checkpoints.insert_routed(
        update_id,
        GatewayDelivery {
            request_id: expected,
            session_id: "session".to_owned(),
            agent_id: "agent".to_owned(),
            message_id: "telegram:10:42".to_owned(),
            sender_id: "7".to_owned(),
            source: "sender".to_owned(),
            conversation_id: "10".to_owned(),
            text: "body".to_owned(),
        },
    );
    let original = serde_json::to_value(&state).expect("encode durable state");
    let invalid_report_ids = [
        "malformed".to_owned(),
        format!("telegram-report:{}", "a".repeat(64)),
        TelegramReportId::for_gateway(
            "other-stream",
            TelegramUpdateId::new(43).expect("valid update"),
        )
        .as_str()
        .to_owned(),
    ];

    for report_id in invalid_report_ids {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("state.json");
        let mut candidate = original.clone();
        candidate["checkpoints"][0]["checkpoint"]["delivery"]["request_id"] =
            serde_json::Value::String(report_id);
        fs::write(
            &path,
            serde_json::to_vec(&candidate).expect("encode corrupt state"),
        )
        .expect("write corrupt state");

        assert!(GatewayDurableState::load(&path, "stream").is_err());
    }
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
/// through the same active chat before routing commands run.
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
    assert!(sent[0].1.contains("gateway is running"));
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
    writeln!(client, r#"{{"protocol_version":0,"kind":"status"}}"#).expect("write request");

    assert!(
        read_gateway_socket_request(&server)
            .expect("status request should parse")
            .is_some()
    );
}

/// Local gateway requests must state the current protocol version explicitly.
#[test]
fn local_socket_rejects_missing_protocol_version() {
    let (mut client, server) = UnixStream::pair().expect("socket pair");
    writeln!(client, r#"{{"kind":"status"}}"#).expect("write request");

    let error =
        read_gateway_socket_request(&server).expect_err("missing protocol version should fail");

    assert!(error.contains("protocol_version"), "{error}");
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
    let status = Arc::new(GatewaySocketState::new(
        &cfg,
        &durable,
        "test-stream".to_owned(),
        PathBuf::from("/tmp/test.sock"),
        Arc::new(FakeGatewayClient::default()),
    ));
    let (mut client, server) = UnixStream::pair().expect("socket pair");
    writeln!(client, r#"{{"protocol_version":0,"kind":"status"}}"#).expect("write request");

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
    assert_eq!(value["routing"], "commands-enabled");
}

/// Ensures sidecar hello advertises that clients must reannounce their live
/// registrations after connecting to this gateway process.
#[test]
fn sidecar_hello_requires_registration_reannouncement() {
    let cfg = runtime_config(Some(10), [7]);
    let durable = GatewayDurableState {
        stream_hash: "test-stream".to_owned(),
        ..GatewayDurableState::default()
    };
    let state = GatewaySocketState::new(
        &cfg,
        &durable,
        "test-stream".to_owned(),
        PathBuf::from("/tmp/test.sock"),
        Arc::new(FakeGatewayClient::default()),
    );

    let response = handle_gateway_socket_request(
        &state,
        1,
        GatewaySocketRequest {
            kind: "hello".to_owned(),
            ..GatewaySocketRequest::default()
        },
    );

    assert_eq!(response["reannounce_required"], true);
    assert_eq!(
        response["heartbeat_interval_seconds"],
        SIDECAR_HEARTBEAT_INTERVAL.as_secs()
    );
    assert_eq!(
        response["registration_lease_seconds"],
        REGISTRATION_LEASE_DURATION.as_secs()
    );
}

/// Ensures disconnecting a sidecar prunes every route it registered so stale
/// session/agent targets cannot remain selectable.
#[test]
fn sidecar_disconnect_prunes_registered_routes() {
    let mut registry = GatewayRegistry::default();
    let now = Instant::now();
    registry.hello(1, now);
    registry
        .register_agent(1, register_request("session-a", "agent-a"), now)
        .expect("register route");
    registry
        .register_agent(1, register_request("session-a", "agent-b"), now)
        .expect("register route");

    registry.disconnect(1);

    let counts = registry.counts(now);
    assert_eq!(counts.sidecars, 0);
    assert_eq!(counts.registrations, 0);
}

/// Ensures registration leases expire when heartbeats stop, while a heartbeat
/// refresh extends the lease for owned registrations.
#[test]
fn sidecar_registration_lease_expires_without_heartbeat() {
    let mut registry = GatewayRegistry::default();
    let now = Instant::now();
    registry.hello(1, now);
    registry
        .register_agent(1, register_request("session-a", "agent-a"), now)
        .expect("register route");
    registry
        .heartbeat(1, now + Duration::from_secs(5))
        .expect("heartbeat refreshes lease");

    registry.prune_expired(now + REGISTRATION_LEASE_DURATION + Duration::from_secs(1));
    assert_eq!(registry.counts(now).registrations, 1);

    registry.prune_expired(now + REGISTRATION_LEASE_DURATION + Duration::from_secs(6));
    assert_eq!(registry.counts(now).registrations, 0);
}

/// Ensures malformed socket traffic closes the connection and removes expired
/// registrations immediately instead of preserving stale routes until status.
#[test]
fn malformed_socket_request_disconnects_and_prunes_stale_routes() {
    let state = Arc::new(test_socket_state());
    let expired_at = Instant::now() - REGISTRATION_LEASE_DURATION - Duration::from_secs(1);
    {
        let mut registry = state.registry.lock().expect("registry lock");
        registry.hello(1, expired_at);
        registry
            .register_agent(1, register_request("session-a", "agent-a"), expired_at)
            .expect("register route");
    }
    let (mut client, server) = UnixStream::pair().expect("socket pair");
    writeln!(client, "not-json").expect("write malformed request");

    handle_gateway_socket_client(server, Arc::clone(&state));

    let mut response = String::new();
    client
        .read_to_string(&mut response)
        .expect("read malformed response");
    let value: serde_json::Value = serde_json::from_str(&response).expect("response JSON");
    assert_eq!(value["ok"], false);
    assert_eq!(state.registry_counts().registrations, 0);
}

/// Ensures the persistent sidecar protocol supports explicit unregister while
/// status reflects live registration counts.
#[test]
fn sidecar_register_unregister_and_status_update_counts() {
    let state = test_socket_state();
    let connection_id = 1;
    handle_gateway_socket_request(
        &state,
        connection_id,
        GatewaySocketRequest {
            kind: "hello".to_owned(),
            ..GatewaySocketRequest::default()
        },
    );
    handle_gateway_socket_request(
        &state,
        connection_id,
        register_request("session-a", "agent-a"),
    );

    let registered = handle_gateway_socket_request(
        &state,
        connection_id,
        GatewaySocketRequest {
            kind: "status".to_owned(),
            ..GatewaySocketRequest::default()
        },
    );
    assert_eq!(registered["active_registration_count"], 1);

    let unregister = GatewaySocketRequest {
        kind: "unregister_agent".to_owned(),
        session_id: Some("session-a".to_owned()),
        agent_id: Some("agent-a".to_owned()),
        ..GatewaySocketRequest::default()
    };
    let unregister_response = handle_gateway_socket_request(&state, connection_id, unregister);
    assert_eq!(unregister_response["ok"], true);

    let unregistered = handle_gateway_socket_request(
        &state,
        connection_id,
        GatewaySocketRequest {
            kind: "status".to_owned(),
            ..GatewaySocketRequest::default()
        },
    );
    assert_eq!(unregistered["active_registration_count"], 0);
}

/// Ensures goodbye on a persistent sidecar socket closes the connection and
/// prunes routes that were still registered by that sidecar.
#[test]
fn sidecar_goodbye_disconnect_prunes_registered_routes() {
    let state = Arc::new(test_socket_state());
    let (mut client, server) = UnixStream::pair().expect("socket pair");
    writeln!(client, r#"{{"protocol_version":0,"kind":"hello"}}"#).expect("write hello");
    writeln!(
        client,
        r#"{{"protocol_version":0,"kind":"register_agent","session_id":"session-a","agent_id":"agent-a"}}"#
    )
    .expect("write register");
    writeln!(client, r#"{{"protocol_version":0,"kind":"goodbye"}}"#).expect("write goodbye");

    handle_gateway_socket_client(server, Arc::clone(&state));

    let mut response = String::new();
    client
        .read_to_string(&mut response)
        .expect("read goodbye responses");
    assert!(response.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line).expect("response JSON")["goodbye"] == true
    }));
    assert_eq!(state.registry_counts().registrations, 0);
}

/// Ensures `/sessions` lists gateway-local aliases rather than full Tau
/// session ids, preserving session privacy while still enabling selection.
#[test]
fn routing_sessions_lists_aliases_without_full_session_ids() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-secret-alpha", "agent-a");
    fixture.register_route(2, "session-secret-beta", "agent-b");

    fixture
        .gateway
        .process_update(update(20, message(7, 10, "/sessions")))
        .expect("process sessions command");

    let sent = fixture.client.sent.lock().expect("sent lock");
    assert_eq!(sent.len(), 1);
    assert!(sent[0].1.contains("s1"));
    assert!(sent[0].1.contains("s2"));
    assert!(!sent[0].1.contains("session-secret-alpha"));
    assert!(!sent[0].1.contains("session-secret-beta"));
}

/// Ensures explicit selection by safe session/agent aliases allows later plain
/// Telegram text to be queued for the owning sidecar without an ambiguity
/// reply.
#[test]
fn routing_selection_queues_plain_text_delivery() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");

    fixture
        .gateway
        .process_update(update(21, message(7, 10, "/select-session s1")))
        .expect("select session");
    fixture
        .gateway
        .process_update(update(22, message(7, 10, "/select a1")))
        .expect("select agent");
    fixture
        .gateway
        .process_update(update(23, message(7, 10, "hello from telegram")))
        .expect("route plain text");

    let deliveries = fixture.take_deliveries(1);
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].session_id, "session-alpha");
    assert_eq!(deliveries[0].agent_id, "agent-alpha");
    assert_eq!(deliveries[0].source, "tester");
    assert_eq!(deliveries[0].sender_id, "7");
    assert_eq!(deliveries[0].conversation_id, "10");
    assert_eq!(deliveries[0].message_id, "telegram:10:23");
    assert_eq!(deliveries[0].text, "hello from telegram");
}

/// Ensures ambiguous plain text is rejected with a Telegram reply instead of
/// being guessed across multiple live gateway registrations.
#[test]
fn routing_plain_text_requires_unambiguous_target() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture.register_route(2, "session-beta", "agent-beta");

    fixture
        .gateway
        .process_update(update(24, message(7, 10, "ambiguous")))
        .expect("process ambiguous text");

    assert!(fixture.take_deliveries(1).is_empty());
    assert!(fixture.take_deliveries(2).is_empty());
    let sent = fixture.client.sent.lock().expect("sent lock");
    assert!(sent[0].1.contains("ambiguous"));
}

/// Ensures a route selected by one Telegram user/chat is not reused by another
/// configured-chat user, preventing cross-user target inheritance.
#[test]
fn routing_selection_is_scoped_to_chat_user() {
    let mut fixture = GatewayFixture::new(Some(10), [7, 8]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture.register_route(2, "session-beta", "agent-beta");
    fixture
        .gateway
        .process_update(update(25, message(7, 10, "/select-session s1")))
        .expect("select session");
    fixture
        .gateway
        .process_update(update(26, message(7, 10, "/select a1")))
        .expect("select agent");

    fixture
        .gateway
        .process_update(update(27, message(8, 10, "do not inherit")))
        .expect("route other user text");

    assert!(fixture.take_deliveries(1).is_empty());
    assert!(fixture.take_deliveries(2).is_empty());
    let sent = fixture.client.sent.lock().expect("sent lock");
    assert!(sent.iter().any(|(_, text)| text.contains("ambiguous")));
}

/// Ensures live aliases remain bound to their originally listed routes when
/// other sessions churn, instead of being recomputed by snapshot position.
#[test]
fn routing_session_aliases_survive_registry_churn() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture.register_route(2, "session-beta", "agent-beta");
    fixture.unregister_route(1, "session-alpha", "agent-alpha");

    fixture
        .gateway
        .process_update(update(28, message(7, 10, "/select-session s1")))
        .expect("select stale alias");
    fixture
        .gateway
        .process_update(update(29, message(7, 10, "/select-session s2")))
        .expect("select live alias");

    let sent = fixture.client.sent.lock().expect("sent lock");
    assert!(
        sent.iter()
            .any(|(_, text)| text.contains("Unknown session alias"))
    );
    assert!(sent.iter().any(|(_, text)| text.contains("Selected")));
}

/// Ensures unregister suppresses a durable delivery until its exact route
/// becomes live again, without deleting the checkpoint.
#[test]
fn routing_unregister_suppresses_pending_delivery() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture
        .gateway
        .process_update(update(30, message(7, 10, "queued")))
        .expect("queue route");

    fixture.unregister_route(1, "session-alpha", "agent-alpha");

    assert!(fixture.take_deliveries(1).is_empty());
}

/// Ensures one socket response exposes at most the bounded durable prefix.
#[test]
fn routing_pending_delivery_queue_is_bounded() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    for update_id in 31..(31 + MAX_PENDING_DELIVERIES_PER_SIDECAR as i64) {
        fixture
            .gateway
            .process_update(update(update_id, message(7, 10, "queued")))
            .expect("queue route");
    }
    fixture
        .gateway
        .process_update(update(99, message(7, 10, "overflow")))
        .expect("overflow route");

    let deliveries = fixture.take_deliveries(1);
    assert_eq!(deliveries.len(), MAX_PENDING_DELIVERIES_PER_SIDECAR);
    assert!(fixture.client.sent.lock().expect("sent lock").is_empty());
}

/// Ensures queued inbound delivery records are exposed through the persistent
/// sidecar socket response shape and replayed until canonical ACK.
#[test]
fn sidecar_heartbeat_replays_pending_delivery_response() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture
        .gateway
        .process_update(update(100, message(7, 10, "queued")))
        .expect("queue route");

    let response = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "heartbeat".to_owned(),
            ..GatewaySocketRequest::default()
        },
    );
    let deliveries = response["deliveries"].as_array().expect("deliveries array");
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0]["session_id"], "session-alpha");
    assert_eq!(deliveries[0]["agent_id"], "agent-alpha");

    let replayed = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "heartbeat".to_owned(),
            ..GatewaySocketRequest::default()
        },
    );
    assert_eq!(
        replayed["deliveries"]
            .as_array()
            .expect("deliveries array")
            .len(),
        1
    );
}

/// Ensures a canonical ACK carries its frozen route and remains valid after
/// that route retires, while a mismatched route cannot mutate durable state.
#[test]
fn canonical_ack_after_route_retirement_requires_exact_frozen_route() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture
        .gateway
        .process_update(update(150, message(7, 10, "routed")))
        .expect("persist routed checkpoint");
    let delivery = fixture.take_deliveries(1).pop().expect("pending delivery");
    fixture.unregister_route(1, "session-alpha", "agent-alpha");

    let mismatched = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "ack_delivery".to_owned(),
            report_id: Some(delivery.request_id.as_str().to_owned()),
            session_id: Some("session-alpha".to_owned()),
            agent_id: Some("agent-other".to_owned()),
            ..GatewaySocketRequest::default()
        },
    );
    assert_eq!(mismatched["ok"], false);
    assert_eq!(
        fixture
            .gateway
            .socket_state
            .durable_deliveries
            .lock()
            .expect("durable deliveries lock")
            .len(),
        1
    );

    let acknowledged = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "ack_delivery".to_owned(),
            report_id: Some(delivery.request_id.as_str().to_owned()),
            session_id: Some("session-alpha".to_owned()),
            agent_id: Some("agent-alpha".to_owned()),
            ..GatewaySocketRequest::default()
        },
    );
    assert_eq!(acknowledged["ok"], true);
    assert!(
        fixture
            .gateway
            .socket_state
            .durable_deliveries
            .lock()
            .expect("durable deliveries lock")
            .is_empty()
    );
}

/// Ensures a routed checkpoint blocks the durable cursor across a later
/// completed command, then one persisted canonical ACK advances the mixed
/// prefix and survives restart.
#[test]
fn canonical_ack_advances_durable_mixed_prefix() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture
        .gateway
        .process_update(update(200, message(7, 10, "routed")))
        .expect("persist routed checkpoint");
    fixture
        .gateway
        .process_update(update(201, message(7, 10, "/status")))
        .expect("persist non-routed checkpoint");

    assert_eq!(fixture.gateway.durable.next_update_offset, None);
    let delivery = fixture.take_deliveries(1).pop().expect("pending delivery");
    fixture
        .gateway
        .socket_state
        .acknowledge_delivery(
            delivery.request_id.as_str().to_owned(),
            GatewayRegistrationKey {
                session_id: "session-alpha".to_owned(),
                agent_id: "agent-alpha".to_owned(),
            },
        )
        .expect("persist acknowledgement");
    fixture.gateway.durable = fixture
        .gateway
        .durable_store
        .snapshot()
        .expect("healthy store");

    assert_eq!(fixture.gateway.durable.next_update_offset, Some(202));
    assert!(fixture.take_deliveries(1).is_empty());
    let restored = GatewayDurableState::load(
        &fixture._tempdir.path().join("state.json"),
        &fixture.gateway.durable.stream_hash,
    )
    .expect("restart durable state");
    assert_eq!(restored.next_update_offset, Some(202));
    assert!(restored.checkpoints.pending_deliveries().is_empty());
}

/// Ensures every routed-update save cut either cleanly rolls back before
/// installation or poisons after installation while restart sees the candidate.
#[test]
fn routed_checkpoint_save_cuts_have_deterministic_recovery() {
    for cut in [
        GatewaySaveCut::Write,
        GatewaySaveCut::FileSync,
        GatewaySaveCut::Rename,
        GatewaySaveCut::ParentSync,
    ] {
        let mut fixture = GatewayFixture::new(Some(10), [7]);
        fixture.register_route(1, "session-alpha", "agent-alpha");
        let before = fixture
            .gateway
            .durable_store
            .snapshot()
            .expect("healthy store");
        fixture.gateway.durable_store.fail_next_save_at(cut);

        let error = fixture
            .gateway
            .process_update(update(205, message(7, 10, "routed")))
            .expect_err("injected routed checkpoint save failure");
        let restored =
            GatewayDurableState::load(&fixture._tempdir.path().join("state.json"), "test-stream")
                .expect("restart state");

        assert!(fixture.take_deliveries(1).is_empty(), "cut: {cut:?}");
        if cut == GatewaySaveCut::ParentSync {
            assert!(error.contains("commit-unknown"), "cut: {cut:?}");
            assert!(fixture.gateway.durable_store.snapshot().is_err());
            assert_eq!(restored.processed_update_count, 1);
            assert_eq!(restored.checkpoints.pending_deliveries().len(), 1);
        } else {
            assert_eq!(
                fixture
                    .gateway
                    .durable_store
                    .snapshot()
                    .expect("healthy rollback"),
                before,
                "cut: {cut:?}"
            );
            assert_eq!(restored, before, "cut: {cut:?}");
        }
    }
}

/// Ensures every ACK save cut either keeps the old pending state or poisons
/// after installation while restart sees the committed ACK.
#[test]
fn canonical_ack_save_cuts_have_deterministic_recovery() {
    for cut in [
        GatewaySaveCut::Write,
        GatewaySaveCut::FileSync,
        GatewaySaveCut::Rename,
        GatewaySaveCut::ParentSync,
    ] {
        let mut fixture = GatewayFixture::new(Some(10), [7]);
        fixture.register_route(1, "session-alpha", "agent-alpha");
        fixture
            .gateway
            .process_update(update(206, message(7, 10, "routed")))
            .expect("persist routed checkpoint");
        let before = fixture
            .gateway
            .durable_store
            .snapshot()
            .expect("healthy store");
        let delivery = fixture.take_deliveries(1).pop().expect("pending delivery");
        fixture.gateway.durable_store.fail_next_save_at(cut);

        let error = fixture
            .gateway
            .socket_state
            .acknowledge_delivery(
                delivery.request_id.as_str().to_owned(),
                GatewayRegistrationKey {
                    session_id: "session-alpha".to_owned(),
                    agent_id: "agent-alpha".to_owned(),
                },
            )
            .expect_err("injected canonical ACK save failure");
        let restored =
            GatewayDurableState::load(&fixture._tempdir.path().join("state.json"), "test-stream")
                .expect("restart state");

        assert_eq!(fixture.take_deliveries(1).len(), 1, "cut: {cut:?}");
        if cut == GatewaySaveCut::ParentSync {
            assert!(error.contains("commit-unknown"), "cut: {cut:?}");
            assert!(fixture.gateway.durable_store.snapshot().is_err());
            assert_eq!(restored.next_update_offset, Some(207));
            assert!(restored.checkpoints.pending_deliveries().is_empty());
            assert_eq!(restored.recent_acknowledgements.len(), 1);
        } else {
            assert_eq!(
                fixture
                    .gateway
                    .durable_store
                    .snapshot()
                    .expect("healthy rollback"),
                before,
                "cut: {cut:?}"
            );
            assert_eq!(restored, before, "cut: {cut:?}");
        }
    }
}

/// Ensures an update waiter that passed an initial health check before another
/// transaction poisoned the store rechecks health after acquiring the lock and
/// cannot mutate or save the commit-unknown state.
#[test]
fn commit_unknown_poison_stops_waiter_after_initial_health_check() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture
        .gateway
        .process_update(update(206, message(7, 10, "routed")))
        .expect("persist routed checkpoint");
    let report_id = fixture
        .take_deliveries(1)
        .pop()
        .expect("pending delivery")
        .request_id;
    let route = GatewayRegistrationKey {
        session_id: "session-alpha".to_owned(),
        agent_id: "agent-alpha".to_owned(),
    };
    let later_update = TelegramUpdateId::new(207).expect("valid later update");
    let mut candidate = fixture
        .gateway
        .durable_store
        .snapshot()
        .expect("healthy store");
    candidate.remember_update(later_update.as_i64());
    candidate.processed_update_count = candidate.processed_update_count.saturating_add(1);
    candidate.checkpoints.insert_non_routed(later_update);

    let ack_entered = Arc::new(Barrier::new(2));
    let ack_resume = Arc::new(Barrier::new(2));
    fixture
        .gateway
        .durable_store
        .pause_next_ack_after_locked_health_check(GatewayStorePause {
            entered: Arc::clone(&ack_entered),
            resume: Arc::clone(&ack_resume),
        });
    fixture
        .gateway
        .durable_store
        .fail_next_save_at(GatewaySaveCut::ParentSync);
    let ack_store = Arc::clone(&fixture.gateway.durable_store);
    let ack_thread = std::thread::spawn(move || ack_store.acknowledge_delivery(&report_id, &route));
    ack_entered.wait();

    let waiter_entered = Arc::new(Barrier::new(2));
    let waiter_resume = Arc::new(Barrier::new(2));
    fixture
        .gateway
        .durable_store
        .pause_next_after_initial_health_check(GatewayStorePause {
            entered: Arc::clone(&waiter_entered),
            resume: Arc::clone(&waiter_resume),
        });
    let waiter_store = Arc::clone(&fixture.gateway.durable_store);
    let waiter_thread =
        std::thread::spawn(move || waiter_store.commit_processed_update(&candidate, later_update));
    waiter_entered.wait();
    waiter_resume.wait();
    ack_resume.wait();

    let ack_error = ack_thread
        .join()
        .expect("ACK thread")
        .expect_err("parent sync must be commit-unknown");
    let waiter_error = waiter_thread
        .join()
        .expect("waiter thread")
        .expect_err("poisoned waiter must fail");
    assert!(ack_error.contains("commit-unknown"));
    assert!(waiter_error.contains("commit-unknown"));

    let state_path = fixture._tempdir.path().join("state.json");
    let exit = fixture
        .gateway
        .run_with_retry(|_| panic!("poisoned state must exit before polling"))
        .expect_err("poisoned runtime state must terminate");
    assert_eq!(exit.exit_code(), ExitCode::from(74));

    let restored = GatewayDurableState::load(&state_path, "test-stream").expect("restart state");
    assert_eq!(restored.next_update_offset, Some(207));
    assert_eq!(restored.processed_update_count, 1);
    assert!(!restored.has_recent_update(207));
    assert_eq!(restored.recent_acknowledgements.len(), 1);
}

/// Ensures a committed exact ACK remains idempotent after its response is
/// dropped, its route retires, and the gateway restarts.
#[test]
fn committed_ack_retry_survives_response_loss_and_restart() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture
        .gateway
        .process_update(update(207, message(7, 10, "routed")))
        .expect("persist routed checkpoint");
    let report_id = fixture
        .take_deliveries(1)
        .pop()
        .expect("pending delivery")
        .request_id;

    let _dropped_response = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "ack_delivery".to_owned(),
            report_id: Some(report_id.as_str().to_owned()),
            session_id: Some("session-alpha".to_owned()),
            agent_id: Some("agent-alpha".to_owned()),
            ..GatewaySocketRequest::default()
        },
    );
    fixture.unregister_route(1, "session-alpha", "agent-alpha");
    fixture
        .gateway
        .socket_state
        .acknowledge_delivery(
            report_id.as_str().to_owned(),
            GatewayRegistrationKey {
                session_id: "session-alpha".to_owned(),
                agent_id: "agent-alpha".to_owned(),
            },
        )
        .expect("retry committed ACK after route retirement");

    let path = fixture._tempdir.path().join("state.json");
    let restored = GatewayDurableState::load(&path, "test-stream").expect("restart durable state");
    assert_eq!(restored.next_update_offset, Some(208));
    assert!(restored.checkpoints.pending_deliveries().is_empty());
    assert_eq!(restored.recent_acknowledgements.len(), 1);

    let restarted_store = Arc::new(GatewayDurableStore::new(path, restored.clone()));
    let restarted_socket = GatewaySocketState::new_with_durable_store(
        &fixture.gateway.cfg,
        &restored,
        "test-stream".to_owned(),
        PathBuf::from("/tmp/restarted-test.sock"),
        Arc::clone(&fixture.gateway.client),
        restarted_store,
    );
    restarted_socket
        .acknowledge_delivery(
            report_id.as_str().to_owned(),
            GatewayRegistrationKey {
                session_id: "session-alpha".to_owned(),
                agent_id: "agent-alpha".to_owned(),
            },
        )
        .expect("retry committed ACK after restart");
}

/// Ensures an ACK racing a separately processed update is retained when that
/// update commits, preserving one contiguous mixed prefix.
#[test]
fn processed_update_commit_preserves_concurrent_ack() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture
        .gateway
        .process_update(update(208, message(7, 10, "routed")))
        .expect("persist routed checkpoint");
    let report_id = fixture
        .take_deliveries(1)
        .pop()
        .expect("pending delivery")
        .request_id;
    let mut candidate = fixture
        .gateway
        .durable_store
        .snapshot()
        .expect("healthy store");
    let later_update = TelegramUpdateId::new(209).expect("valid later update");
    candidate.remember_update(later_update.as_i64());
    candidate.processed_update_count = candidate.processed_update_count.saturating_add(1);
    candidate.checkpoints.insert_non_routed(later_update);
    let route = GatewayRegistrationKey {
        session_id: "session-alpha".to_owned(),
        agent_id: "agent-alpha".to_owned(),
    };
    fixture
        .gateway
        .durable_store
        .acknowledge_delivery(&report_id, &route)
        .expect("commit racing ACK");

    let committed = fixture
        .gateway
        .durable_store
        .commit_processed_update(&candidate, later_update)
        .expect("commit separately processed update");

    assert_eq!(committed.next_update_offset, Some(210));
    assert!(committed.checkpoints.pending_deliveries().is_empty());
    assert_eq!(committed.recent_acknowledgements.len(), 1);
}

/// Ensures restart and registration churn suppress but never delete or
/// retarget an exact pending routed report.
#[test]
fn restart_and_reregistration_replay_exact_routed_report() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture
        .gateway
        .process_update(update(210, message(7, 10, "exact body")))
        .expect("persist routed checkpoint");
    let original = fixture.take_deliveries(1);
    assert_eq!(original.len(), 1);

    let restored = GatewayDurableState::load(
        &fixture._tempdir.path().join("state.json"),
        &fixture.gateway.durable.stream_hash,
    )
    .expect("restart durable state");
    assert_eq!(restored.checkpoints.pending_deliveries(), original);
    fixture.unregister_route(1, "session-alpha", "agent-alpha");
    assert!(fixture.take_deliveries(1).is_empty());
    fixture.register_route(2, "session-alpha", "agent-alpha");
    assert_eq!(fixture.take_deliveries(2), original);
}

/// Proves the response limit includes the newline and accepts an exact
/// 65,536-byte serialized singleton while rejecting one additional byte.
#[test]
fn delivery_response_limit_has_exact_json_line_boundary() {
    let generation = "test-generation";
    let empty = test_delivery(1, "");
    let fixed_bytes = successful_response_wire_bytes(generation, &[empty]);
    let boundary = test_delivery(1, &"x".repeat(MAX_GATEWAY_RESPONSE_BYTES - fixed_bytes));

    assert_eq!(
        successful_response_wire_bytes(generation, std::slice::from_ref(&boundary)),
        MAX_GATEWAY_RESPONSE_BYTES
    );
    assert!(delivery_response_fits(
        generation,
        std::slice::from_ref(&boundary)
    ));

    let oversized = test_delivery(1, &format!("{}x", boundary.text));
    assert!(!delivery_response_fits(
        generation,
        std::slice::from_ref(&oversized)
    ));
}

/// Ensures JSON escaping drives delivery-prefix selection by actual serialized
/// bytes rather than unescaped Rust string length.
#[test]
fn delivery_batching_accounts_for_json_escaping() {
    assert_delivery_batch_splits_two_records(&"\"\\\n".repeat(6_000));
}

/// Ensures multibyte UTF-8 drives delivery-prefix selection by encoded bytes
/// without splitting or miscounting Unicode scalar values.
#[test]
fn delivery_batching_accounts_for_multibyte_utf8() {
    assert_delivery_batch_splits_two_records(&"界".repeat(11_000));
}

/// Ensures enqueue rejects an impossible singleton with a bounded,
/// content-free outcome rather than leaving an undrainable queue head.
#[test]
fn enqueue_rejects_delivery_that_cannot_fit_one_response() {
    let fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    let private_marker = "PRIVATE-OVERSIZED-CONTENT";
    let text = format!("{}{private_marker}", "x".repeat(MAX_GATEWAY_RESPONSE_BYTES));

    let error = fixture
        .gateway
        .socket_state
        .enqueue_delivery(
            &GatewayRegistrationKey {
                session_id: "session-alpha".to_owned(),
                agent_id: "agent-alpha".to_owned(),
            },
            &message(7, 10, &text),
            TelegramUpdateId::new(101).expect("test update id"),
            &text,
        )
        .expect_err("oversized singleton must be rejected");

    assert_eq!(error, DELIVERY_TOO_LARGE_MESSAGE);
    assert!(error.len() <= MAX_SOCKET_ERROR_BYTES);
    assert!(!error.contains(private_marker));
    assert!(fixture.take_deliveries(1).is_empty());
}

/// Ensures stale-route rejection precedes singleton-size validation so an
/// oversized body does not change existing route-authority diagnostics.
#[test]
fn enqueue_checks_missing_route_before_singleton_size() {
    let fixture = GatewayFixture::new(Some(10), [7]);
    let text = "x".repeat(MAX_GATEWAY_RESPONSE_BYTES);

    let error = fixture
        .gateway
        .socket_state
        .enqueue_delivery(
            &GatewayRegistrationKey {
                session_id: "missing-session".to_owned(),
                agent_id: "missing-agent".to_owned(),
            },
            &message(7, 10, &text),
            TelegramUpdateId::new(102).expect("test update id"),
            &text,
        )
        .expect_err("missing route must be rejected first");

    assert!(error.contains("no longer live"), "{error}");
}

/// Ensures durable backlog depth does not hide singleton-size validation.
#[test]
fn enqueue_checks_full_queue_before_singleton_size() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    for update_id in 1..=MAX_PENDING_DELIVERIES_PER_SIDECAR as i64 {
        fixture
            .gateway
            .process_update(update(update_id, message(7, 10, "queued")))
            .expect("fill pending queue");
    }
    let text = "x".repeat(MAX_GATEWAY_RESPONSE_BYTES);

    let error = fixture
        .gateway
        .socket_state
        .enqueue_delivery(
            &GatewayRegistrationKey {
                session_id: "session-alpha".to_owned(),
                agent_id: "agent-alpha".to_owned(),
            },
            &message(7, 10, &text),
            TelegramUpdateId::new(103).expect("test update id"),
            &text,
        )
        .expect_err("full queue must be rejected first");

    assert_eq!(error, DELIVERY_TOO_LARGE_MESSAGE);
}

/// Reproduces 32 queued 3,500-byte records through the real client,
/// exercises both send and heartbeat response producers, and proves the oldest
/// bounded prefix replays unchanged while canonical ACK is missing.
#[test]
fn gateway_client_replays_bounded_durable_prefix_in_order() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    let tempdir = tempfile::tempdir().expect("socket tempdir");
    let socket_path = tempdir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind gateway socket");
    let state = Arc::clone(&fixture.gateway.socket_state);
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept gateway client");
        handle_gateway_socket_client(stream, state);
    });
    let client = GatewayClient::new(GatewayClientConfig { socket_path });

    let _connected = bounded_gateway_response(client.connect_cancellable(|| false));
    let _registered = bounded_gateway_response(client.register_agent(
        "session-alpha",
        "agent-alpha",
        Some("tester".to_owned()),
    ));

    let text = "x".repeat(3_500);
    for update_id in 1..=MAX_PENDING_DELIVERIES_PER_SIDECAR as i64 {
        fixture
            .gateway
            .process_update(update(update_id, message(7, 10, &text)))
            .expect("queue maximum-size delivery");
    }

    let first =
        bounded_gateway_response(client.send_message("session-alpha", "agent-alpha", "outbound"));
    let request_ids = first
        .deliveries
        .into_iter()
        .map(|delivery| delivery.request_id)
        .collect::<Vec<_>>();
    assert!(
        request_ids.len() < MAX_PENDING_DELIVERIES_PER_SIDECAR,
        "the reproduction must require more than one response"
    );

    let replayed = bounded_gateway_response(client.heartbeat())
        .deliveries
        .into_iter()
        .map(|delivery| delivery.request_id)
        .collect::<Vec<_>>();
    assert_eq!(replayed, request_ids);
    assert_eq!(
        request_ids.iter().collect::<HashSet<_>>().len(),
        request_ids.len()
    );

    drop(client);
    server.join().expect("gateway socket server");
}

/// Ensures outbound gateway sends require a live sidecar-owned registration and
/// choose the gateway's configured chat instead of accepting any model-provided
/// destination.
#[test]
fn outbound_send_uses_configured_chat_for_registered_agent() {
    let fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");

    let response = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "send_message".to_owned(),
            session_id: Some("session-alpha".to_owned()),
            agent_id: Some("agent-alpha".to_owned()),
            message: Some("hello from tau".to_owned()),
            ..GatewaySocketRequest::default()
        },
    );

    assert_eq!(response["ok"], true);
    let sent = fixture.client.sent.lock().expect("sent");
    assert_eq!(
        sent.as_slice(),
        [(10, "[agent-alpha] hello from tau".to_owned())]
    );
}

/// Ensures a sidecar cannot send as an unregistered or stale agent route, which
/// keeps `telegram_send` gated on explicit registration through the gateway.
#[test]
fn outbound_send_rejects_unregistered_agent() {
    let fixture = GatewayFixture::new(Some(10), [7]);

    let response = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "send_message".to_owned(),
            session_id: Some("session-alpha".to_owned()),
            agent_id: Some("agent-alpha".to_owned()),
            message: Some("hello from tau".to_owned()),
            ..GatewaySocketRequest::default()
        },
    );

    assert_eq!(response["ok"], false);
    assert!(
        response["error"]
            .as_str()
            .expect("error")
            .contains("not registered")
    );
    assert!(fixture.client.sent.lock().expect("sent").is_empty());
}

/// Ensures outbound gateway send failures return a bounded generic error to the
/// sidecar while detailed Telegram transport text stays in logs.
#[test]
fn outbound_send_failure_is_sanitized_for_sidecar() {
    let fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture.client.fail_next_sends(1);

    let response = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "send_message".to_owned(),
            session_id: Some("session-alpha".to_owned()),
            agent_id: Some("agent-alpha".to_owned()),
            message: Some("hello from tau".to_owned()),
            ..GatewaySocketRequest::default()
        },
    );

    assert_eq!(response["ok"], false);
    assert_eq!(
        response["error"].as_str(),
        Some("Telegram gateway could not send the message.")
    );
}

/// Ensures a route registered by one sidecar connection cannot be used for
/// outbound sends by another same-UID socket connection.
#[test]
fn outbound_send_rejects_route_owned_by_another_sidecar() {
    let fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");

    let response = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        2,
        GatewaySocketRequest {
            kind: "send_message".to_owned(),
            session_id: Some("session-alpha".to_owned()),
            agent_id: Some("agent-alpha".to_owned()),
            message: Some("hello from tau".to_owned()),
            ..GatewaySocketRequest::default()
        },
    );

    assert_eq!(response["ok"], false);
    assert!(
        response["error"]
            .as_str()
            .expect("error")
            .contains("another sidecar")
    );
    assert!(fixture.client.sent.lock().expect("sent").is_empty());
}

/// Ensures outbound sends use a linked private chat when the gateway has no
/// fixed configured chat.
#[test]
fn outbound_send_uses_linked_chat_without_configured_chat() {
    let cfg = runtime_config(None, [7]);
    let durable = GatewayDurableState {
        stream_hash: "test-stream".to_owned(),
        linked_chat: Some(GatewayLinkedChat {
            chat_id: 77,
            user_id: 7,
        }),
        ..GatewayDurableState::default()
    };
    let fixture = GatewayFixture::with_durable(cfg, durable);
    fixture.register_route(1, "session-alpha", "agent-alpha");

    let response = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "send_message".to_owned(),
            session_id: Some("session-alpha".to_owned()),
            agent_id: Some("agent-alpha".to_owned()),
            message: Some("hello linked chat".to_owned()),
            ..GatewaySocketRequest::default()
        },
    );

    assert_eq!(response["ok"], true);
    let sent = fixture.client.sent.lock().expect("sent");
    assert_eq!(
        sent.as_slice(),
        [(77, "[agent-alpha] hello linked chat".to_owned())]
    );
}

/// Ensures outbound sends fail closed when no configured or linked active chat
/// exists, rather than letting the sidecar provide a destination.
#[test]
fn outbound_send_rejects_missing_active_chat() {
    let fixture = GatewayFixture::new(None, [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");

    let response = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "send_message".to_owned(),
            session_id: Some("session-alpha".to_owned()),
            agent_id: Some("agent-alpha".to_owned()),
            message: Some("hello from tau".to_owned()),
            ..GatewaySocketRequest::default()
        },
    );

    assert_eq!(response["ok"], false);
    assert!(
        response["error"]
            .as_str()
            .expect("error")
            .contains("not linked")
    );
    assert!(fixture.client.sent.lock().expect("sent").is_empty());
}

/// Ensures oversized outbound messages are rejected before Telegram is called.
#[test]
fn outbound_send_rejects_oversized_message() {
    let fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");

    let response = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "send_message".to_owned(),
            session_id: Some("session-alpha".to_owned()),
            agent_id: Some("agent-alpha".to_owned()),
            message: Some("x".repeat(MAX_OUTBOUND_MESSAGE_BYTES + 1)),
            ..GatewaySocketRequest::default()
        },
    );

    assert_eq!(response["ok"], false);
    assert!(
        response["error"]
            .as_str()
            .expect("error")
            .contains("too large")
    );
    assert!(fixture.client.sent.lock().expect("sent").is_empty());
}

/// Ensures the gateway owns a bounded outbound-send rate limit and rejects
/// excess model-authored sends without calling Telegram.
#[test]
fn outbound_send_rate_limit_is_bounded() {
    let fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");

    for index in 0..MAX_OUTBOUND_SENDS_PER_WINDOW {
        let response = handle_gateway_socket_request(
            &fixture.gateway.socket_state,
            1,
            GatewaySocketRequest {
                kind: "send_message".to_owned(),
                session_id: Some("session-alpha".to_owned()),
                agent_id: Some("agent-alpha".to_owned()),
                message: Some(format!("hello {index}")),
                ..GatewaySocketRequest::default()
            },
        );
        assert_eq!(response["ok"], true);
    }

    let response = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "send_message".to_owned(),
            session_id: Some("session-alpha".to_owned()),
            agent_id: Some("agent-alpha".to_owned()),
            message: Some("one too many".to_owned()),
            ..GatewaySocketRequest::default()
        },
    );

    assert_eq!(response["ok"], false);
    assert!(
        response["error"]
            .as_str()
            .expect("error")
            .contains("rate limit")
    );
    assert_eq!(
        fixture.client.sent.lock().expect("sent").len(),
        MAX_OUTBOUND_SENDS_PER_WINDOW
    );
}

/// Ensures a transient Telegram send failure is an operation error only: it is
/// reported to the sidecar but does not close the persistent socket or prune
/// the sidecar's registrations.
#[test]
fn outbound_send_failure_keeps_sidecar_connection_live() {
    let fixture = GatewayFixture::new(Some(10), [7]);
    fixture.client.fail_next_sends(1);
    let state = Arc::clone(&fixture.gateway.socket_state);
    let (mut client, server) = UnixStream::pair().expect("socket pair");
    let server_thread = std::thread::spawn(move || handle_gateway_socket_client(server, state));

    writeln!(client, r#"{{"protocol_version":0,"kind":"hello"}}"#).expect("write hello");
    assert_socket_ok(&mut client);
    writeln!(
        client,
        r#"{{"protocol_version":0,"kind":"register_agent","session_id":"session-alpha","agent_id":"agent-alpha"}}"#
    )
    .expect("write register");
    assert_socket_ok(&mut client);
    writeln!(
        client,
        r#"{{"protocol_version":0,"kind":"send_message","session_id":"session-alpha","agent_id":"agent-alpha","message":"first"}}"#
    )
    .expect("write send");
    let failed = read_socket_json(&mut client);
    assert_eq!(failed["ok"], false);
    assert_eq!(failed["keep_connection"], true);
    writeln!(client, r#"{{"protocol_version":0,"kind":"heartbeat"}}"#).expect("write heartbeat");
    assert_socket_ok(&mut client);
    writeln!(
        client,
        r#"{{"protocol_version":0,"kind":"send_message","session_id":"session-alpha","agent_id":"agent-alpha","message":"second"}}"#
    )
    .expect("write second send");
    assert_socket_ok(&mut client);

    drop(client);
    server_thread.join().expect("socket server");
    let sent = fixture.client.sent.lock().expect("sent");
    assert_eq!(sent.as_slice(), [(10, "[agent-alpha] second".to_owned())]);
}

/// Ensures `/agents [session]` renders live agent aliases in the requested
/// session without requiring it to be selected first.
#[test]
fn routing_agents_command_accepts_session_argument() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha-long", "agent-alpha-long");

    fixture
        .gateway
        .process_update(update(101, message(7, 10, "/agents session-alpha")))
        .expect("agents command");

    let sent = fixture.client.sent.lock().expect("sent lock");
    assert!(sent[0].1.contains("a1"));
    assert!(sent[0].1.contains("agent-alpha"));
}

/// Ensures `/where` reports the selected route after session and agent
/// selection.
#[test]
fn routing_where_reports_selected_route() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha-long", "agent-alpha-long");
    fixture
        .gateway
        .process_update(update(102, message(7, 10, "/select-session s1")))
        .expect("select session");
    fixture
        .gateway
        .process_update(update(103, message(7, 10, "/select a1")))
        .expect("select agent");
    fixture
        .gateway
        .process_update(update(104, message(7, 10, "/where")))
        .expect("where command");

    let sent = fixture.client.sent.lock().expect("sent lock");
    assert!(sent.iter().any(|(_, text)| text.contains("session-alph")));
    assert!(sent.iter().any(|(_, text)| text.contains("agent-alpha")));
}

/// Ensures `/to <session>/<agent> <message>` queues a delivery using explicit
/// session and agent selectors.
#[test]
fn routing_to_explicit_session_agent_queues_delivery() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha-long", "agent-alpha-long");

    fixture
        .gateway
        .process_update(update(
            105,
            message(7, 10, "/to session-alpha/agent-alpha explicit"),
        ))
        .expect("explicit to route");

    let deliveries = fixture.take_deliveries(1);
    assert_eq!(deliveries.len(), 1);
    assert!(deliveries[0].text.contains("explicit"));
}

/// Ensures `/to <agent> <message>` resolves the agent within the selected
/// session.
#[test]
fn routing_to_selected_session_agent_queues_delivery() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha-long", "agent-alpha-long");
    fixture
        .gateway
        .process_update(update(106, message(7, 10, "/select-session s1")))
        .expect("select session");

    fixture
        .gateway
        .process_update(update(107, message(7, 10, "/to agent-alpha selected")))
        .expect("selected to route");

    let deliveries = fixture.take_deliveries(1);
    assert_eq!(deliveries.len(), 1);
    assert!(deliveries[0].text.contains("selected"));
}

/// Ensures stable id prefixes route only when unambiguous and produce an
/// ambiguity reply otherwise.
#[test]
fn routing_stable_prefixes_must_be_unambiguous() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha-one", "agent-alpha-one");
    fixture.register_route(1, "session-alpha-one", "agent-alpha-uno");
    fixture.register_route(2, "session-alpha-two", "agent-alpha-two");

    fixture
        .gateway
        .process_update(update(108, message(7, 10, "/select-session session-alpha")))
        .expect("ambiguous session prefix");
    fixture
        .gateway
        .process_update(update(
            109,
            message(7, 10, "/select-session session-alpha-o"),
        ))
        .expect("unambiguous session prefix");
    fixture
        .gateway
        .process_update(update(110, message(7, 10, "/select agent-alpha")))
        .expect("ambiguous agent prefix");
    fixture
        .gateway
        .process_update(update(111, message(7, 10, "/select agent-alpha-on")))
        .expect("unambiguous agent prefix");

    let sent = fixture.client.sent.lock().expect("sent lock");
    assert!(
        sent.iter()
            .any(|(_, text)| text.contains("Session selector is ambiguous"))
    );
    assert!(
        sent.iter()
            .any(|(_, text)| text.contains("Agent selector is ambiguous"))
    );
    assert!(
        sent.iter()
            .any(|(_, text)| text.contains("Selected Telegram gateway agent"))
    );
}

/// Ensures Telegram source labels in queued deliveries are bounded and stripped
/// of control characters before becoming fact display metadata.
#[test]
fn routing_source_labels_are_sanitized() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    let mut incoming = message(7, 10, "sanitize");
    incoming.from_name = Some(format!("bad\nname{}", "x".repeat(100)));

    fixture
        .gateway
        .process_update(update(112, incoming))
        .expect("route sanitized source");

    let deliveries = fixture.take_deliveries(1);
    assert_eq!(deliveries.len(), 1);
    assert!(!deliveries[0].source.contains('\n'));
    assert!(deliveries[0].source.len() <= 80);
    assert!(deliveries[0].source.starts_with("badname"));
    assert_eq!(deliveries[0].text, "sanitize");
}

/// Fake Telegram client state captured by gateway tests.
#[derive(Default)]
struct FakeGatewayClient {
    /// Queued update batches returned by polling.
    updates: Mutex<VecDeque<Result<Vec<TgUpdate>, crate::TelegramApiFailure>>>,
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
    fn get_webhook_info(
        &self,
        _cfg: &RuntimeConfig,
    ) -> Result<crate::TgWebhookInfo, crate::TelegramApiFailure> {
        Ok(crate::TgWebhookInfo::default())
    }

    fn get_updates(
        &self,
        _cfg: &RuntimeConfig,
        _offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, crate::TelegramApiFailure> {
        self.updates
            .lock()
            .expect("updates lock")
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    fn send_message(
        &self,
        _cfg: &RuntimeConfig,
        chat_id: i64,
        text: &str,
    ) -> Result<(), crate::TelegramApiFailure> {
        let mut failures = self.send_failures.lock().expect("failures lock");
        if *failures > 0 {
            *failures -= 1;
            return Err(crate::TelegramApiFailure::Protocol(
                "simulated send failure".to_owned(),
            ));
        }
        drop(failures);
        self.sent
            .lock()
            .expect("sent lock")
            .push((chat_id, text.to_owned()));
        Ok(())
    }
}

/// Ordinary runtime failures wait exactly five seconds and repoll, while the
/// following HTTP 409 exits immediately as unavailable.
#[test]
fn runtime_poll_retry_policy_is_deterministic() {
    let fixture = GatewayFixture::new(Some(10), [7]);
    fixture
        .client
        .updates
        .lock()
        .expect("updates lock")
        .extend([
            Err(crate::TelegramApiFailure::Transport),
            Err(crate::TelegramApiFailure::Http {
                status: 500,
                message: "temporary".to_owned(),
            }),
            Err(crate::TelegramApiFailure::Protocol(
                "unexpected shape".to_owned(),
            )),
            Err(crate::TelegramApiFailure::Http {
                status: 409,
                message: "conflict".to_owned(),
            }),
        ]);
    let mut waits = Vec::new();
    let error = fixture
        .gateway
        .run_with_retry(|delay| waits.push(delay))
        .expect_err("HTTP 409 must terminate polling");
    assert_eq!(error.exit_code(), ExitCode::from(69));
    assert_eq!(
        waits,
        [
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(5),
        ]
    );
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
        let state_path = tempdir.path().join("state.json");
        let durable_store = Arc::new(GatewayDurableStore::new(
            state_path.clone(),
            durable.clone(),
        ));
        let socket_state = Arc::new(GatewaySocketState::new_with_durable_store(
            &cfg,
            &durable,
            "test-stream".to_owned(),
            PathBuf::from("/tmp/test.sock"),
            Arc::clone(&gateway_client),
            Arc::clone(&durable_store),
        ));
        Self {
            gateway: Gateway {
                cfg,
                client: gateway_client,
                durable,
                durable_store,
                socket_state,
                _resources: GatewayResources::Test,
            },
            client,
            _tempdir: tempdir,
        }
    }

    /// Register one live route owned by a fake sidecar connection.
    fn register_route(&self, connection_id: u64, session_id: &str, agent_id: &str) {
        let mut registry = self
            .gateway
            .socket_state
            .registry
            .lock()
            .expect("registry lock");
        let now = Instant::now();
        registry.hello(connection_id, now);
        registry
            .register_agent(connection_id, register_request(session_id, agent_id), now)
            .expect("register test route");
    }

    /// Unregister one fake sidecar-owned route.
    fn unregister_route(&self, connection_id: u64, session_id: &str, agent_id: &str) {
        self.gateway
            .socket_state
            .registry
            .lock()
            .expect("registry lock")
            .unregister_agent(
                connection_id,
                GatewaySocketRequest {
                    kind: "unregister_agent".to_owned(),
                    session_id: Some(session_id.to_owned()),
                    agent_id: Some(agent_id.to_owned()),
                    ..GatewaySocketRequest::default()
                },
            )
            .expect("unregister test route");
    }

    /// Drain queued deliveries for a fake sidecar connection.
    fn take_deliveries(&self, connection_id: u64) -> Vec<GatewayDelivery> {
        self.gateway
            .socket_state
            .durable_deliveries_for_connection(connection_id)
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
        update_id: TelegramUpdateId::new(update_id).expect("test update id"),
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

/// Build a gateway socket register_agent request for tests.
fn register_request(session_id: &str, agent_id: &str) -> GatewaySocketRequest {
    GatewaySocketRequest {
        kind: "register_agent".to_owned(),
        session_id: Some(session_id.to_owned()),
        agent_id: Some(agent_id.to_owned()),
        display_name: Some("tester".to_owned()),
        ..GatewaySocketRequest::default()
    }
}

/// Build one deterministic delivery record for response-boundary tests.
fn test_delivery(request_id: u64, text: &str) -> GatewayDelivery {
    GatewayDelivery {
        request_id: TelegramReportId::for_gateway(
            "delivery-response-test",
            TelegramUpdateId::new(request_id as i64).expect("valid update"),
        ),
        session_id: "session-alpha".to_owned(),
        agent_id: "agent-alpha".to_owned(),
        message_id: format!("telegram:10:{request_id}"),
        sender_id: "7".to_owned(),
        source: "tester".to_owned(),
        conversation_id: "10".to_owned(),
        text: text.to_owned(),
    }
}

/// Return exact serialized response bytes, including the JSON-line newline.
fn successful_response_wire_bytes(generation: &str, deliveries: &[GatewayDelivery]) -> usize {
    serde_json::to_vec(&successful_socket_response(generation, deliveries))
        .expect("serialize successful socket response")
        .len()
        + 1
}

/// Prove two individually valid records split after the oldest record.
fn assert_delivery_batch_splits_two_records(text: &str) {
    let generation = "test-generation";
    let first = test_delivery(1, text);
    let second = test_delivery(2, text);
    assert!(delivery_response_fits(
        generation,
        std::slice::from_ref(&first)
    ));
    assert!(delivery_response_fits(
        generation,
        std::slice::from_ref(&second)
    ));
    assert!(!delivery_response_fits(
        generation,
        &[first.clone(), second.clone()]
    ));
    let selected = select_delivery_prefix(generation, &[first.clone(), second]);

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].request_id, first.request_id);
    assert!(successful_response_wire_bytes(generation, &selected) <= MAX_GATEWAY_RESPONSE_BYTES);
}

/// Require the real client to accept a newline-inclusive bounded response.
fn bounded_gateway_response(
    response: Result<GatewaySocketResponse, crate::gateway_client::GatewayClientError>,
) -> GatewaySocketResponse {
    response.expect("real GatewayClient must accept a response within the shared line limit")
}

/// Read one JSON-line response from a gateway socket test client.
fn read_socket_json(stream: &mut UnixStream) -> serde_json::Value {
    let mut line = String::new();
    path_std_io::BufReader::new(stream.try_clone().expect("clone socket client"))
        .read_line(&mut line)
        .expect("read socket response");
    serde_json::from_str(&line).expect("socket response JSON")
}

/// Assert that one gateway socket response is an accepted operation.
fn assert_socket_ok(stream: &mut UnixStream) {
    let response = read_socket_json(stream);
    assert_eq!(response["ok"], true, "{response}");
}

/// Build a gateway socket state for protocol tests.
fn test_socket_state() -> GatewaySocketState {
    let cfg = runtime_config(Some(10), [7]);
    let durable = GatewayDurableState {
        stream_hash: "test-stream".to_owned(),
        ..GatewayDurableState::default()
    };
    GatewaySocketState::new(
        &cfg,
        &durable,
        "test-stream".to_owned(),
        PathBuf::from("/tmp/test.sock"),
        Arc::new(FakeGatewayClient::default()),
    )
}
