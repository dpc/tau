use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use tau_proto::ContextItem;
use tungstenite::Message;

use super::*;
use crate::common::PromptPayload;
use crate::responses::ResponsesSurface;

fn context(items: &[ContextItem]) -> &'static tau_proto::PromptContext {
    Box::leak(Box::new(tau_proto::PromptContext {
        blocks: vec![tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: items.to_vec(),
            },
        )],
    }))
}

fn context_after_response(
    response_id: &str,
    output_items: Vec<ContextItem>,
    after: Vec<ContextItem>,
) -> &'static tau_proto::PromptContext {
    Box::leak(Box::new(tau_proto::PromptContext {
        blocks: vec![
            tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                provider_response_id: Some(response_id.to_owned()),
                backend: None,
                output_items,
                usage: None,
            }),
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock { items: after }),
        ],
    }))
}

#[test]
fn keys_distinguish_agents_under_same_account() {
    let cfg = make_config("https://chatgpt.com/backend-api", Some("acc"));
    let a = pool_key_for(&cfg, "agent-a", tau_proto::PromptOriginator::User, false);
    let b = pool_key_for(&cfg, "agent-b", tau_proto::PromptOriginator::User, false);
    assert_ne!(a, b);
}

#[test]
fn keys_ignore_prompt_originator_buckets() {
    // Upgrade headers are fixed for the socket lifetime, so the pool must follow
    // the prompt-cache UUID exactly. Since the cache key is stable per agent,
    // originator changes and the legacy share-user flag must not split sockets.
    let cfg = make_config("https://chatgpt.com/backend-api", Some("acc"));
    let user = pool_key_for(&cfg, "agent", tau_proto::PromptOriginator::User, false);
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("__harness__"),
        query_id: "delegate-1".into(),
    };
    let default_ext = pool_key_for(&cfg, "agent", ext.clone(), false);
    let shared_ext = pool_key_for(&cfg, "agent", ext, true);

    assert_eq!(user, default_ext);
    assert_eq!(user, shared_ext);
}

#[test]
fn keys_distinguish_accounts_under_same_thread_id() {
    let a = pool_key_for(
        &make_config("https://chatgpt.com/backend-api", Some("acc-1")),
        "agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    let b = pool_key_for(
        &make_config("https://chatgpt.com/backend-api", Some("acc-2")),
        "agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    assert_ne!(a, b);
}

/// The headline pool invariant: alternating between two prompt-cache threads
/// must NOT cause the second thread's turn to flush the first thread's
/// connection. Each `(account, thread-id)` must hold its own socket so the
/// OpenAI connection-local `previous_response_id` cache stays warm across
/// context switches.
#[test]
fn pool_routes_each_thread_to_its_own_socket_and_reuses_them() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    // Two turns on cache bucket A, interleaved with one on cache bucket B.
    // Expected: 2 upgrades total (one per prompt-cache bucket), 3 turns.
    for agent in ["agent-a", "agent-b", "agent-a"] {
        let session_id = tau_proto::SessionId::new("session-pool-routing");
        let agent_id = tau_proto::AgentId::parse(agent).expect("agent id");
        let request = PromptPayload {
            system_prompt: "sys",
            context: context(&[]),
            tools: &[],
            params: tau_proto::ModelParams::default(),
            tool_choice: tau_proto::ToolChoice::default(),
            compaction: None,
            originator: &tau_proto::PromptOriginator::User,
            session_id: &session_id,
            agent_id: &agent_id,
            share_user_cache_key: false,
            debug_provider_requests: false,
        };
        run_turn_through_pool(
            &mut pool,
            &config,
            "session-pool-routing",
            "sp-test",
            &request,
            &mut on_update,
        )
        .expect("turn ok");
    }

    let state = server.lock().expect("server state lock");
    assert_eq!(
        state.upgrade_count, 2,
        "expected one upgrade per distinct prompt-cache thread (alternating A/B/A — reuses A's socket)"
    );
    assert_eq!(
        state.turns_per_connection,
        vec![2, 1],
        "thread A's socket should have served two turns; thread B's, one"
    );
}

/// Concurrent same-key turns must serialize at the shared-pool reservation
/// boundary. Without that reservation, both workers can observe an empty
/// map while the first turn owns the socket and open two sockets for one
/// conversation chain.
#[test]
fn shared_pool_serializes_same_key_turns() {
    let (addr, server) = spawn_fake_codex_server();
    server.lock().expect("server lock").response_delay = Duration::from_millis(100);
    let config = Arc::new(make_config(
        &format!("http://{addr}/backend-api"),
        Some("acc"),
    ));
    let pool = Arc::new(SharedWsPool::new());
    let barrier = Arc::new(Barrier::new(2));

    let mut handles = Vec::new();
    for idx in 0..2 {
        let config = config.clone();
        let pool = pool.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            run_shared_turn(&pool, &config, "same-session", &format!("sp-{idx}"));
        }));
    }
    for handle in handles {
        handle.join().expect("worker join");
    }

    let state = server.lock().expect("server state lock");
    assert_eq!(
        state.upgrade_count, 1,
        "same PoolKey must reuse one reserved socket rather than opening a parallel chain"
    );
    assert_eq!(state.turns_per_connection, vec![2]);
    assert_eq!(
        state.max_active_turns, 1,
        "same-key WS turns should run one at a time"
    );
}

/// A prompt canceled while it is queued behind a same-key WS reservation
/// must stop waiting instead of sending a stale network request after the
/// active turn releases. The pool polls prompt cancellation while parked on
/// the condvar so the waiter can unwind and let the worker emit its
/// terminal canceled response/PromptDone.
#[test]
fn shared_pool_checkout_wait_aborts_when_canceled() {
    let config = make_config("https://chatgpt.com/backend-api", Some("acc"));
    let key = pool_key_for(
        &config,
        "test-agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    let pool = Arc::new(SharedWsPool::new());
    pool.inner
        .lock()
        .expect("pool lock")
        .busy
        .insert(key.clone());

    let canceled = Arc::new(AtomicBool::new(false));
    let started = Arc::new(Barrier::new(2));
    let handle = {
        let pool = pool.clone();
        let key = key.clone();
        let canceled = canceled.clone();
        let started = started.clone();
        thread::spawn(move || {
            started.wait();
            pool.checkout_until(&key, "test", &mut || canceled.load(Ordering::SeqCst))
        })
    };

    started.wait();
    thread::sleep(Duration::from_millis(100));
    canceled.store(true, Ordering::SeqCst);

    let result = handle.join().expect("checkout waiter join");
    assert!(matches!(result, Err(WsTurnError::Canceled)));
    assert!(
        pool.inner.lock().expect("pool lock").busy.contains(&key),
        "a canceled waiter must not steal or clear the active worker's reservation"
    );
}

/// Prewarm runs on the provider main loop, so it must never park behind an
/// active same-key reservation. A busy key means a real turn is already
/// doing the warming work; skip best-effort prewarm instead of delaying
/// cancellation, worker output, PromptDone, or ACK processing.
#[test]
fn shared_prewarm_skips_busy_same_key_without_waiting() {
    let (addr, _server) = spawn_fake_codex_server();
    let config = Arc::new(make_config(
        &format!("http://{addr}/backend-api"),
        Some("acc"),
    ));
    let pool = Arc::new(SharedWsPool::new());
    let key = pool_key_for(
        &config,
        "test-agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    pool.inner
        .lock()
        .expect("pool lock")
        .busy
        .insert(key.clone());

    let (tx, rx) = std::sync::mpsc::channel();
    let handle = {
        let config = config.clone();
        let pool = pool.clone();
        thread::spawn(move || {
            let session_id = tau_proto::SessionId::new("same-session");
            let originator = tau_proto::PromptOriginator::User;
            let request = PromptPayload {
                system_prompt: "sys",
                context: context(&[]),
                tools: &[],
                params: tau_proto::ModelParams::default(),
                tool_choice: tau_proto::ToolChoice::default(),
                compaction: None,
                originator: &originator,
                session_id: &session_id,
                agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
                share_user_cache_key: false,
                debug_provider_requests: false,
            };
            let started = std::time::Instant::now();
            let result = run_prewarm_through_shared_pool(&pool, &config, "same-session", &request);
            tx.send((started.elapsed(), result.is_ok()))
                .expect("send prewarm result");
        })
    };

    let (elapsed, ok) = match rx.recv_timeout(Duration::from_millis(150)) {
        Ok(result) => result,
        Err(error) => {
            pool.inner.lock().expect("pool lock").busy.remove(&key);
            pool.changed.notify_all();
            handle.join().expect("prewarm join after unblocking");
            panic!("prewarm blocked on a busy same-key reservation: {error}");
        }
    };
    handle.join().expect("prewarm join");

    assert!(ok, "skipped prewarm should report success");
    assert!(
        elapsed < Duration::from_millis(50),
        "prewarm should not wait for the checkout poll interval; elapsed {elapsed:?}"
    );
    assert!(
        pool.inner.lock().expect("pool lock").busy.contains(&key),
        "skipped prewarm must not clear the active worker's reservation"
    );
    assert_eq!(
        pool.stats().expect("pool stats").upgrades,
        0,
        "skipped prewarm should not open a socket"
    );
}

/// Different prompt-cache thread keys should not be serialized by the same-key
/// guard. The shared mutex may protect bookkeeping, but it must not cover
/// network I/O.
#[test]
fn shared_pool_allows_different_keys_to_run_concurrently() {
    let (addr, server) = spawn_fake_codex_server();
    server.lock().expect("server lock").response_delay = Duration::from_millis(150);
    let config = Arc::new(make_config(
        &format!("http://{addr}/backend-api"),
        Some("acc"),
    ));
    let pool = Arc::new(SharedWsPool::new());
    let barrier = Arc::new(Barrier::new(2));

    let mut handles = Vec::new();
    for (idx, agent) in ["agent-a", "agent-b"].into_iter().enumerate() {
        let config = config.clone();
        let pool = pool.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            run_shared_turn_for_agent(
                &pool,
                &config,
                "session-shared-different-keys",
                agent,
                &format!("sp-{idx}"),
            );
        }));
    }
    for handle in handles {
        handle.join().expect("worker join");
    }

    let state = server.lock().expect("server state lock");
    assert_eq!(
        state.upgrade_count, 2,
        "different keys use different sockets"
    );
    assert_eq!(
        state.max_active_turns, 2,
        "different-key WS network turns should overlap"
    );
}

/// Cap the pool at 2 and exercise three agents. The least-recently-used
/// agent's socket must get evicted; a follow-up turn on that agent triggers a
/// fresh upgrade.
#[test]
fn pool_evicts_lru_when_capacity_exceeded() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    pool.conns
        .resize(NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN));
    let mut on_update = |_: &crate::common::StreamState| {};

    // A → B → C: three different agents/cache buckets, cap=2.
    // After C: A (LRU) is evicted, pool holds {B, C}.
    for agent in ["agent-a", "agent-b", "agent-c"] {
        run_turn_for_agent(&mut pool, &config, "session-lru", agent, &mut on_update);
    }
    assert_eq!(pool.len(), 2);
    assert_eq!(server.lock().expect("server state lock").upgrade_count, 3);

    // Touching A again must re-upgrade (its old socket got
    // evicted on C's release).
    run_turn_for_agent(&mut pool, &config, "session-lru", "agent-a", &mut on_update);
    assert_eq!(server.lock().expect("server state lock").upgrade_count, 4);
}

/// Connections older than `MAX_CONNECTION_AGE` must be
/// pre-emptively reopened on checkout, so the server's 60-min
/// hard cap never fires mid-turn.
#[test]
fn pool_reopens_aged_out_connections_on_checkout() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    // First turn opens connection #1.
    run_turn(&mut pool, &config, "session-aged", &mut on_update);
    assert_eq!(server.lock().expect("server state lock").upgrade_count, 1);

    // Forcibly age the cached connection past the threshold.
    let key = pool_key_for(
        &config,
        "test-agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    if let Some(conn) = pool.conns.get_mut(&key) {
        conn.opened_at = std::time::Instant::now() - MAX_CONNECTION_AGE - Duration::from_secs(1);
    } else {
        panic!("expected connection in pool");
    }

    // Next turn must reopen rather than send on the stale socket.
    run_turn(&mut pool, &config, "session-aged", &mut on_update);
    assert_eq!(
        server.lock().expect("server state lock").upgrade_count,
        2,
        "aged-out connection should have been replaced"
    );
}

/// HTTP+SSE base + plain TCP fake server doubles as the WS
/// transport's smoke test: connect, send a turn, read all the
/// expected events back, see `response_id` captured.
#[test]
fn ws_turn_captures_response_id_for_chain_continuation() {
    let (addr, _server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    let mut last_text = String::new();
    let mut on_update = |state: &crate::common::StreamState| {
        last_text = state.text.clone();
    };

    let session_id = tau_proto::SessionId::new("session-x");
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &session_id,
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let state = run_turn_through_pool(
        &mut pool,
        &config,
        "session-x",
        "sp-test",
        &request,
        &mut on_update,
    )
    .expect("turn ok");
    assert_eq!(last_text, "hello");
    assert!(
        state.response_id.is_some(),
        "response_id must be captured so the next turn can chain via previous_response_id"
    );
}

/// ChatGPT requires the WebSocket upgrade to identify the upstream session and
/// thread before any `response.create` frame is sent. Those headers must use
/// the exact same UUID as the request body's `prompt_cache_key`; otherwise a
/// pooled socket could be bound to one upstream thread while the turn body
/// targets a different cache bucket.
#[test]
fn ws_upgrade_thread_headers_match_prompt_cache_key() {
    let (addr, server) = spawn_fake_codex_server();
    let mut config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    config.supports_prompt_cache_key = true;
    let mut pool = WsPool::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    let session_id = tau_proto::SessionId::new("session-headers");
    let agent_id = tau_proto::AgentId::parse("header-agent").expect("agent id");
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &session_id,
        agent_id: &agent_id,
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let expected = request.prompt_cache_key(&config.base_url);

    run_turn_through_pool(
        &mut pool,
        &config,
        "session-headers",
        "sp-test",
        &request,
        &mut on_update,
    )
    .expect("turn ok");

    let s = server.lock().expect("server lock");
    let headers = s.upgrade_headers.first().expect("captured upgrade headers");
    assert_eq!(headers.get("session-id"), Some(&expected));
    assert_eq!(headers.get("thread-id"), Some(&expected));
    assert_eq!(
        s.requests[0]
            .get("prompt_cache_key")
            .and_then(serde_json::Value::as_str),
        Some(expected.as_str())
    );
}

/// A `generate:false` prewarm warms the provider cache for the prompt-cache
/// key/thread-id. It no longer acts as a synthetic `previous_response_id` chain
/// anchor; the next real turn sends a full prompt and relies on cache hits.
#[test]
fn prewarm_warms_cache_without_chaining_next_turn() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let session_id = tau_proto::SessionId::new("session-prewarm");
    let prewarmed_messages = vec![user_msg("AGENTS.md context")];
    let real_messages = vec![user_msg("AGENTS.md context"), user_msg("actual request")];

    let prewarm = PromptPayload {
        system_prompt: "sys",
        context: context(&prewarmed_messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &session_id,
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    run_prewarm_through_pool(&mut pool, &config, "session-prewarm", &prewarm).expect("prewarm ok");

    let real = PromptPayload {
        context: context(&real_messages),
        debug_provider_requests: false,
        ..prewarm
    };
    run_turn_through_pool(
        &mut pool,
        &config,
        "session-prewarm",
        "sp-test",
        &real,
        &mut on_update,
    )
    .expect("turn ok");

    let s = server.lock().expect("server lock");
    assert_eq!(s.upgrade_count, 1, "prewarm and turn must share one socket");
    assert_eq!(s.requests.len(), 2, "expected prewarm plus real turn");
    let warm = &s.requests[0];
    let turn = &s.requests[1];
    assert_eq!(
        warm.get("generate").and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert!(
        turn.get("previous_response_id").is_none(),
        "prewarm is cache-only and must not become a synthetic chain anchor",
    );
    assert_eq!(
        turn.get("input")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(2),
        "real turn should send the full prompt after cache-only prewarm",
    );
}

/// Codex's WS `previous_response_id` cache is connection-local. When the
/// pool opens a fresh socket for a chained turn, the new socket has no
/// knowledge of the prior response id. The pool strips the id, replays the
/// full prompt once over WS, and keeps the fresh socket warm for the next
/// turn instead of sticky-falling back to HTTP.
#[test]
fn fresh_open_with_previous_response_rebuilds_ws_warmth() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    let session_id = tau_proto::SessionId::new("session-fresh");
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &session_id,
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    run_turn_through_pool(
        &mut pool,
        &config,
        "session-fresh",
        "sp-test",
        &request,
        &mut on_update,
    )
    .expect("fresh chained WS turn should rebuild warmth");

    let s = server.lock().expect("server lock");
    assert_eq!(s.upgrade_count, 1, "must open a replacement WS socket");
    assert_eq!(s.requests.len(), 1, "expected one WS full replay envelope");
    assert!(
        s.requests[0].get("previous_response_id").is_none(),
        "fresh WS socket must not receive a stale chain id"
    );
}

#[test]
fn fresh_open_with_previous_response_preserves_compacted_items() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let session_id = tau_proto::SessionId::new("session-compacted");
    let messages = vec![
        tau_proto::ContextItem::Compaction(tau_proto::OpaqueProviderItem(
            crate::common::json_to_cbor(&serde_json::json!({
                "type": "message",
                "role": "user",
                "content": "compacted-sentinel",
            })),
        )),
        user_msg("after compaction"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &session_id,
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    run_turn_through_pool(
        &mut pool,
        &config,
        "session-compacted",
        "sp-test",
        &request,
        &mut on_update,
    )
    .expect("fresh chained WS turn should replay compacted context");

    let s = server.lock().expect("server lock");
    let input = s.requests[0]
        .get("input")
        .and_then(serde_json::Value::as_array)
        .expect("input array");
    assert!(
        input.iter().any(
            |item| item.get("content").and_then(serde_json::Value::as_str)
                == Some("compacted-sentinel")
        ),
        "fresh WS replay must keep compacted input items when stripping the stale chain id",
    );
}

/// A cached connection dies mid-turn (keepalive timeout / TCP reset). If
/// the request has a `previous_response_id`, the pool must reopen a fresh
/// WS socket, strip the stale chain id, and leave the replacement socket in
/// the pool so later turns regain cache warmth.
#[test]
fn mid_stream_close_with_chain_rebuilds_ws_warmth() {
    let (addr, server) = spawn_fake_codex_server();
    // Make connection #0 die mid-turn-#2 (after_turn=1 -> the
    // second arriving turn on conn 0 is the one that gets closed).
    server.lock().expect("server lock").fault = Some(MidStreamCloseFault {
        on_conn_index: 0,
        after_turn: 1,
    });
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    // Turn 1: opens conn-0, returns a `response_id` the harness
    // would chain off for turn 2.
    let session_id = tau_proto::SessionId::new("session-die");
    let req1 = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &session_id,
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let state1 = run_turn_through_pool(
        &mut pool,
        &config,
        "session-die",
        "sp-test-1",
        &req1,
        &mut on_update,
    )
    .expect("first turn ok");
    let prev_id = state1.response_id.expect("first turn yielded response_id");

    // Turn 2: harness wants to chain via `prev_id`. The cached socket dies
    // mid-stream; pool must reopen cold WS and strip the chain id rather
    // than sticky-disabling WS for the session.
    let req2 = PromptPayload {
        system_prompt: "sys",
        context: context_after_response(&prev_id, Vec::new(), vec![user_msg("second turn")]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &session_id,
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    run_turn_through_pool(
        &mut pool,
        &config,
        "session-die",
        "sp-test-2",
        &req2,
        &mut on_update,
    )
    .expect("chained reconnect should rebuild WS warmth");

    let s = server.lock().expect("server lock");
    assert_eq!(
        s.upgrade_count, 2,
        "mid-stream close should force one replacement WS upgrade"
    );
    // Three captured requests in arrival order:
    //   #0: turn-1 on conn-0 (no chain id, no prior response)
    //   #1: turn-2 on conn-0 (had chain id; this is the one that died)
    //   #2: turn-2 replay on conn-1 (chain stripped for fresh WS)
    assert_eq!(s.requests.len(), 3, "expected one WS replay envelope");
    assert!(
        s.requests[1].get("previous_response_id").is_some(),
        "turn-2 on the warm socket should still carry the chain id (warm cache path)"
    );
    assert!(
        s.requests[2].get("previous_response_id").is_none(),
        "replacement socket must not receive a stale chain id"
    );
    assert_eq!(
        pool.stats().silent_reconnects,
        1,
        "stat counter should record the silent reconnect"
    );
}

/// Shared-pool reconnect must keep the same key reserved while a recoverable
/// cached-socket failure is replayed on a fresh socket. Otherwise a racing
/// same-key worker could open a competing chain between the failed cached turn
/// and the replacement release.
#[test]
fn shared_pool_mid_stream_close_keeps_reservation_through_fresh_retry() {
    let (addr, server) = spawn_fake_codex_server();
    server.lock().expect("server lock").fault = Some(MidStreamCloseFault {
        on_conn_index: 0,
        after_turn: 1,
    });
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let pool = SharedWsPool::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    let session_id = tau_proto::SessionId::new("session-shared-die");
    let agent_id = tau_proto::AgentId::parse("test-agent").expect("agent id");
    let originator = tau_proto::PromptOriginator::User;
    let req1 = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &originator,
        session_id: &session_id,
        agent_id: &agent_id,
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let state1 = run_turn_through_shared_pool(
        &pool,
        &config,
        "sp-shared-1",
        &req1,
        &mut || false,
        &mut on_update,
    )
    .expect("first shared turn ok");
    let prev_id = state1.response_id.expect("first turn yielded response_id");

    let req2 = PromptPayload {
        system_prompt: "sys",
        context: context_after_response(&prev_id, Vec::new(), vec![user_msg("second turn")]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &originator,
        session_id: &session_id,
        agent_id: &agent_id,
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    run_turn_through_shared_pool(
        &pool,
        &config,
        "sp-shared-2",
        &req2,
        &mut || false,
        &mut on_update,
    )
    .expect("shared chained reconnect should rebuild WS warmth");

    let s = server.lock().expect("server lock");
    assert_eq!(
        s.upgrade_count, 2,
        "shared reconnect should open exactly one replacement socket"
    );
    assert_eq!(
        s.requests.len(),
        3,
        "expected cached failure plus fresh replay"
    );
    assert!(
        s.requests[1].get("previous_response_id").is_some(),
        "cached warm turn should carry the chain id before the socket dies"
    );
    assert!(
        s.requests[2].get("previous_response_id").is_none(),
        "fresh shared retry must strip the stale chain id"
    );
    drop(s);

    let stats = pool.stats().expect("pool stats");
    assert_eq!(
        stats.silent_reconnects, 1,
        "shared pool should count the cached recoverable reconnect"
    );
}

/// Every error shape `WsConn::run_turn` can emit must be
/// classified recoverable so the silent-reconnect path catches
/// it. The old narrow allow-list (`"ws closed"` /
/// `"previous_response"` / `"response not found"`) silently
/// missed `"ws writer task gone"` and `"ws keepalive failed:
/// ..."` after the tokio-tungstenite refactor — a dead cached
/// socket would then leak its error to the user instead of
/// being reopened transparently. Guards against re-tightening.
#[test]
fn all_run_turn_error_shapes_are_recoverable() {
    let cases = [
        "stream error: ws closed",
        "stream error: ws closed mid-stream (code=1011 reason=keepalive ping timeout)",
        "stream error: ws writer task gone",
        "stream error: ws reader task gone",
        "stream error: ws send failed: Connection closed normally",
        "stream error: ws keepalive failed: IO error: broken pipe",
        "stream error: Previous response not found",
        "stream error: previous_response_id expired",
        "stream error: response not found",
        "stream error: WebSocket protocol error: bad frame",
    ];
    for body in cases {
        let err = LlmError::HttpStatus(0, body.to_owned());
        assert!(
            is_recoverable_ws_error(&err),
            "expected recoverable: {body}"
        );
    }
}

/// Inverse: only `HttpStatus(0, "stream error: ...")` is in
/// scope. Other code paths (real HTTP errors, JSON failures,
/// non-stream `HttpStatus(0, ...)` bodies) must not be
/// transparently retried — they could be terminal user-facing
/// problems (bad auth, malformed request) where reopening the
/// socket changes nothing.
#[test]
fn non_run_turn_errors_are_not_recoverable() {
    let cases = [
        LlmError::HttpStatus(0, "response failed: model overloaded".to_owned()),
        LlmError::HttpStatus(401, "Unauthorized".to_owned()),
        LlmError::HttpStatus(429, "rate limit".to_owned()),
        LlmError::HttpStatus(0, "some unrelated body".to_owned()),
    ];
    for err in cases {
        assert!(
            !is_recoverable_ws_error(&err),
            "expected NOT recoverable: {err:?}"
        );
    }
}

/// Account-level caps (usage_limit_reached etc.) ride the same
/// `stream error: …` envelope as transport hiccups but are NOT
/// fixable by reopening the socket. The pool must surface them
/// up to `LlmError::retry_after` (which also returns `None` for
/// these) instead of burning a fresh upgrade.
#[test]
fn account_limit_stream_errors_are_not_silent_reconnects() {
    let cases = [
        "stream error: usage limit (type=usage_limit_reached)",
        "stream error: rate limit (type=rate_limit_exceeded)",
        "stream error: quota (type=quota_exceeded)",
    ];
    for body in cases {
        let err = LlmError::HttpStatus(0, body.to_owned());
        assert!(
            !is_recoverable_ws_error(&err),
            "account cap must short-circuit, not silent-reconnect: {body}",
        );
    }
}

// -----------------------------------------------------------------
// Fake Codex server: minimal blocking tungstenite acceptor.
// -----------------------------------------------------------------

#[derive(Default)]
struct ServerState {
    /// How many TCP+upgrade pairs we've accepted. Each
    /// `(account, thread-id)` pair the pool keys against should
    /// produce exactly one upgrade across its lifetime (modulo
    /// age-out / OAuth refresh).
    upgrade_count: usize,
    /// Upgrade request headers captured for each accepted WebSocket.
    upgrade_headers: Vec<BTreeMap<String, String>>,
    /// `turns_per_connection[i]` is the number of
    /// `response.create` envelopes connection `i` served before
    /// closing. Lets pool-reuse tests assert that A's two turns
    /// landed on one socket.
    turns_per_connection: Vec<usize>,
    /// Captured request bodies, in arrival order across all
    /// connections. Available for tests that want to inspect
    /// what the client actually sent (chain ids, model knobs).
    requests: Vec<serde_json::Value>,
    /// Artificial per-turn response delay used by concurrency tests to make
    /// overlapping network turns observable.
    response_delay: Duration,
    /// Number of fake server turns currently sleeping/streaming.
    active_turns: usize,
    /// Maximum simultaneous fake server turns observed during a test.
    max_active_turns: usize,
    /// Fault injection. When `Some`, the worker for a matching
    /// connection drops the socket with a 1011 close frame
    /// instead of serving the offending turn — mimicking the
    /// "keepalive ping timeout" the live Codex server produces
    /// when its idle reaper fires. Tests use this to exercise
    /// the silent-reconnect path.
    fault: Option<MidStreamCloseFault>,
}

/// "After connection index `on_conn_index` has fully served
/// `after_turn` turns, drop the next incoming turn mid-stream."
/// Indices are zero-based; `after_turn: 1` means the second
/// arriving turn on that connection is the one that gets killed.
#[derive(Clone, Copy)]
struct MidStreamCloseFault {
    on_conn_index: usize,
    after_turn: usize,
}

fn spawn_fake_codex_server() -> (SocketAddr, Arc<Mutex<ServerState>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let state = Arc::new(Mutex::new(ServerState::default()));
    let state_clone = state.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let conn_state = state_clone.clone();
            thread::spawn(move || handle_one_connection(stream, conn_state));
        }
    });
    (addr, state)
}

fn capture_headers(headers: &tungstenite::http::HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

fn handle_one_connection(stream: TcpStream, state: Arc<Mutex<ServerState>>) {
    let mut upgrade_headers = BTreeMap::new();
    let mut ws = match tungstenite::accept_hdr(
        stream,
        #[allow(clippy::result_large_err)]
        |request: &tungstenite::handshake::server::Request, response| {
            upgrade_headers = capture_headers(request.headers());
            Ok(response)
        },
    ) {
        Ok(ws) => ws,
        Err(_) => return,
    };
    let conn_idx;
    {
        let mut s = state.lock().expect("server state lock");
        s.upgrade_count += 1;
        s.upgrade_headers.push(upgrade_headers);
        conn_idx = s.turns_per_connection.len();
        s.turns_per_connection.push(0);
    }

    let mut turn_counter = 0_usize;
    loop {
        let msg = match ws.read() {
            Ok(m) => m,
            Err(_) => return,
        };
        match msg {
            Message::Text(text) => {
                let parsed: serde_json::Value =
                    serde_json::from_str(text.as_str()).unwrap_or(serde_json::Value::Null);
                let (fault_now, response_delay) = {
                    let mut s = state.lock().expect("server state lock");
                    s.requests.push(parsed.clone());
                    s.turns_per_connection[conn_idx] += 1;
                    s.active_turns += 1;
                    s.max_active_turns = s.max_active_turns.max(s.active_turns);
                    let fault_now = s
                        .fault
                        .filter(|f| f.on_conn_index == conn_idx && turn_counter >= f.after_turn);
                    (fault_now, s.response_delay)
                };
                turn_counter += 1;
                if !response_delay.is_zero() {
                    thread::sleep(response_delay);
                }
                if fault_now.is_some() {
                    // Mimic the live Codex 1011 keepalive-timeout
                    // drop: send a close frame and bail without
                    // streaming the response body. Client side
                    // sees `Message::Close` → `LlmError(0, "stream
                    // error: ws closed mid-stream ...")`.
                    let _ = ws.send(Message::Close(Some(tungstenite::protocol::CloseFrame {
                        code: tungstenite::protocol::frame::coding::CloseCode::Error,
                        reason: "keepalive ping timeout".into(),
                    })));
                    finish_server_turn(&state);
                    return;
                }
                // Stream a tiny canned event sequence: one
                // visible-text delta, then completed.
                let events = [
                    serde_json::json!({
                        "type": "response.output_text.delta",
                        "delta": "hello",
                    }),
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "id": format!("resp_{conn_idx}_{turn_counter}"),
                            "usage": {
                                "input_tokens": 1,
                                "output_tokens": 1,
                                "input_tokens_details": { "cached_tokens": 0 },
                            },
                        },
                    }),
                ];
                for ev in events {
                    let txt = serde_json::to_string(&ev).expect("serialize");
                    if ws.send(Message::Text(txt.into())).is_err() {
                        finish_server_turn(&state);
                        return;
                    }
                }
                finish_server_turn(&state);
            }
            Message::Close(_) => return,
            _ => continue,
        }
    }
}

fn finish_server_turn(state: &Arc<Mutex<ServerState>>) {
    let mut s = state.lock().expect("server state lock");
    s.active_turns = s.active_turns.saturating_sub(1);
}

fn pool_key_for(
    config: &ResponsesConfig,
    agent: &str,
    originator: tau_proto::PromptOriginator,
    share_user_cache_key: bool,
) -> PoolKey {
    let session_id = tau_proto::SessionId::new("test-session");
    let agent_id = tau_proto::AgentId::parse(agent).expect("agent id");
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &originator,
        session_id: &session_id,
        agent_id: &agent_id,
        share_user_cache_key,
        debug_provider_requests: false,
    };
    PoolKey::for_request(config, &request)
}

fn user_msg(text: &str) -> tau_proto::ContextItem {
    tau_proto::ContextItem::Message(tau_proto::MessageItem {
        role: tau_proto::ContextRole::User,
        content: vec![tau_proto::ContentPart::Text {
            text: text.to_owned(),
        }],
        phase: None,
    })
}

fn run_turn(
    pool: &mut WsPool,
    config: &ResponsesConfig,
    session: &str,
    on_update: &mut impl FnMut(&crate::common::StreamState),
) {
    run_turn_for_agent(pool, config, session, "test-agent", on_update);
}

fn run_turn_for_agent(
    pool: &mut WsPool,
    config: &ResponsesConfig,
    session: &str,
    agent: &str,
    on_update: &mut impl FnMut(&crate::common::StreamState),
) {
    let session_id = tau_proto::SessionId::new(session);
    let agent_id = tau_proto::AgentId::parse(agent).expect("agent id");
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &session_id,
        agent_id: &agent_id,
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    run_turn_through_pool(pool, config, session, "sp-test", &request, on_update).expect("turn ok");
}

fn run_shared_turn(
    pool: &SharedWsPool,
    config: &ResponsesConfig,
    session: &str,
    agent_prompt_id: &str,
) {
    run_shared_turn_for_agent(pool, config, session, "test-agent", agent_prompt_id);
}

fn run_shared_turn_for_agent(
    pool: &SharedWsPool,
    config: &ResponsesConfig,
    session: &str,
    agent: &str,
    agent_prompt_id: &str,
) {
    let session_id = tau_proto::SessionId::new(session);
    let agent_id = tau_proto::AgentId::parse(agent).expect("agent id");
    let originator = tau_proto::PromptOriginator::User;
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &originator,
        session_id: &session_id,
        agent_id: &agent_id,
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let mut on_update = |_: &crate::common::StreamState| {};
    run_turn_through_shared_pool(
        pool,
        config,
        agent_prompt_id,
        &request,
        &mut || false,
        &mut on_update,
    )
    .expect("shared turn ok");
}

fn make_config(base_url: &str, account_id: Option<&str>) -> ResponsesConfig {
    ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        base_url: base_url.into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        context_window: 258400,
        account_id: account_id.map(str::to_owned),
        supports_reasoning_effort: false,
        supports_reasoning_summary: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_websocket: true,
        supports_compaction: false,
        supports_prompt_cache_key: false,
        supports_encrypted_reasoning: false,
    }
}
