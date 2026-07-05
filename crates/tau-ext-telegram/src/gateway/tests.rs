use std::collections::VecDeque;
use std::io::{BufRead as _, Read as _, Write as _};

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
        selected_route: None,
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
    writeln!(client, r#"{{"protocol_version":1,"kind":"status"}}"#).expect("write request");

    assert!(
        read_gateway_socket_request(&server)
            .expect("status request should parse")
            .is_some()
    );
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
    writeln!(client, r#"{{"protocol_version":1,"kind":"hello"}}"#).expect("write hello");
    writeln!(
        client,
        r#"{{"protocol_version":1,"kind":"register_agent","session_id":"session-a","agent_id":"agent-a"}}"#
    )
    .expect("write register");
    writeln!(client, r#"{{"protocol_version":1,"kind":"goodbye"}}"#).expect("write goodbye");

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
    assert!(deliveries[0].text.contains("[telegram from tester]"));
    assert!(deliveries[0].text.contains("hello from telegram"));
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

/// Ensures pending deliveries are removed if a route unregisters before the
/// sidecar drains them, so stale prompts are not delivered after ownership
/// loss.
#[test]
fn routing_unregister_drops_pending_delivery() {
    let mut fixture = GatewayFixture::new(Some(10), [7]);
    fixture.register_route(1, "session-alpha", "agent-alpha");
    fixture
        .gateway
        .process_update(update(30, message(7, 10, "queued")))
        .expect("queue route");

    fixture.unregister_route(1, "session-alpha", "agent-alpha");

    assert!(fixture.take_deliveries(1).is_empty());
}

/// Ensures a sidecar cannot accumulate an unbounded inbound delivery queue.
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
    let sent = fixture.client.sent.lock().expect("sent lock");
    assert!(sent.iter().any(|(_, text)| text.contains("queue is full")));
}

/// Ensures queued prompt deliveries are exposed through the persistent sidecar
/// socket response shape and drained after one successful response.
#[test]
fn sidecar_heartbeat_drains_queued_delivery_response() {
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

    let drained = handle_gateway_socket_request(
        &fixture.gateway.socket_state,
        1,
        GatewaySocketRequest {
            kind: "heartbeat".to_owned(),
            ..GatewaySocketRequest::default()
        },
    );
    assert_eq!(
        drained["deliveries"]
            .as_array()
            .expect("deliveries array")
            .len(),
        0
    );
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

    writeln!(client, r#"{{"protocol_version":1,"kind":"hello"}}"#).expect("write hello");
    assert_socket_ok(&mut client);
    writeln!(
        client,
        r#"{{"protocol_version":1,"kind":"register_agent","session_id":"session-alpha","agent_id":"agent-alpha"}}"#
    )
    .expect("write register");
    assert_socket_ok(&mut client);
    writeln!(
        client,
        r#"{{"protocol_version":1,"kind":"send_message","session_id":"session-alpha","agent_id":"agent-alpha","message":"first"}}"#
    )
    .expect("write send");
    let failed = read_socket_json(&mut client);
    assert_eq!(failed["ok"], false);
    assert_eq!(failed["keep_connection"], true);
    writeln!(client, r#"{{"protocol_version":1,"kind":"heartbeat"}}"#).expect("write heartbeat");
    assert_socket_ok(&mut client);
    writeln!(
        client,
        r#"{{"protocol_version":1,"kind":"send_message","session_id":"session-alpha","agent_id":"agent-alpha","message":"second"}}"#
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
/// of control characters before being reflected in prompt text.
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
    assert!(deliveries[0].text.contains("[telegram from badname"));
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
        let socket_state = Arc::new(GatewaySocketState::new(
            &cfg,
            &durable,
            "test-stream".to_owned(),
            PathBuf::from("/tmp/test.sock"),
            Arc::clone(&gateway_client),
        ));
        Self {
            gateway: Gateway {
                cfg,
                client: gateway_client,
                state_path: tempdir.path().join("state.json"),
                durable,
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
            .registry
            .lock()
            .expect("registry lock")
            .take_deliveries(connection_id)
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

/// Build a gateway socket register_agent request for tests.
fn register_request(session_id: &str, agent_id: &str) -> GatewaySocketRequest {
    GatewaySocketRequest {
        kind: "register_agent".to_owned(),
        session_id: Some(session_id.to_owned()),
        agent_id: Some(agent_id.to_owned()),
        display_name: Some("tester".to_owned()),
        tool_namespace: Some("telegram".to_owned()),
        ..GatewaySocketRequest::default()
    }
}

/// Read one JSON-line response from a gateway socket test client.
fn read_socket_json(stream: &mut UnixStream) -> serde_json::Value {
    let mut line = String::new();
    std::io::BufReader::new(stream.try_clone().expect("clone socket client"))
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
