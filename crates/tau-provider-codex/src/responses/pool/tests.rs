use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc as std_mpsc};
use std::{sync as path_std_sync, thread, time as path_std_time};

use tau_proto::ContextItem;
use tungstenite::protocol::frame::coding as path_tungstenite_protocol_frame_coding;
use tungstenite::{Message, handshake as path_tungstenite_handshake};

use super::*;
use crate::common::PromptPayload;
use crate::responses::ResponsesMode;
use crate::{NeverAbort, TurnAbort, TurnAbortWaker};

type TestAbortWakerSlot = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync + 'static>>>>;

struct AtomicAbort {
    canceled: Arc<AtomicBool>,
    waker: TestAbortWakerSlot,
    registered_tx: std_mpsc::Sender<()>,
}

impl TurnAbort for AtomicAbort {
    fn is_aborted(&mut self) -> bool {
        self.canceled.load(Ordering::SeqCst)
    }

    fn register_waker(
        &mut self,
        waker: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker> {
        *self.waker.lock().expect("abort waker lock") = Some(waker);
        self.registered_tx.send(()).expect("registered receiver");
        Box::new(TestAbortWaker)
    }
}

struct TestAbortWaker;

impl TurnAbortWaker for TestAbortWaker {}

/// Guard that exposes and pauses its drop for publication-order tests.
struct BlockingDropWaker {
    /// Announces that callback unregistration reached guard drop.
    entered: std_mpsc::Sender<()>,
    /// Releases guard drop so physical publication may finish.
    release: std_mpsc::Receiver<()>,
}

impl Drop for BlockingDropWaker {
    fn drop(&mut self) {
        self.entered.send(()).expect("announce guard drop");
        self.release.recv().expect("release guard drop");
    }
}

impl TurnAbortWaker for BlockingDropWaker {}

fn context(items: &[ContextItem]) -> &'static tau_proto::PromptContext {
    Box::leak(Box::new(tau_proto::PromptContext {
        blocks: vec![tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: items.to_vec(),
            },
        )],
    }))
}

fn block_context(blocks: Vec<tau_proto::ContextBlock>) -> &'static tau_proto::PromptContext {
    Box::leak(Box::new(tau_proto::PromptContext { blocks }))
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
        name: tau_proto::ExtensionName::parse("__harness__")
            .expect("test extension name must satisfy the identifier grammar"),
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
        let session_id = tau_proto::SessionId::parse("session-pool-routing")
            .expect("known-safe SessionId must be valid");
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

    let state = server.lock_state();
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

/// Ensures a provider-authored usage-window error received from an actual local
/// WebSocket reaches the common classifier without a silent reconnect.
#[test]
fn local_websocket_usage_window_contract_returns_typed_retry() {
    let (addr, server) = spawn_fake_codex_server();
    server.lock_state().scripted_error = Some(serde_json::json!({
        "type": "error",
        "error": {
            "code": "usage_limit_reached",
            "message": "weekly allocation exhausted",
            "resets_in_seconds": 432000
        }
    }));
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let session_id = tau_proto::SessionId::parse("session-wire-error")
        .expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("agent-wire-error").expect("agent id");
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
    let mut pool = WsPool::new();
    let error = match run_turn_through_pool(
        &mut pool,
        &config,
        "session-wire-error",
        "ap-wire-error",
        &request,
        &mut |_| {},
    ) {
        Err(error) => error.into_llm_error(),
        Ok(_) => panic!("scripted WebSocket error must fail this attempt"),
    };
    let decision = error
        .retry_decision()
        .expect("usage-window error remains scheduler-owned");
    assert_eq!(
        decision.class,
        tau_provider::retry_policy::RetryClass::UsageWindow
    );
    assert_eq!(decision.retry_after, Some(Duration::from_secs(432_000)));
    assert_eq!(
        server.lock_state().upgrade_count,
        1,
        "account errors must not trigger a silent reconnect"
    );
}

/// Concurrent same-key turns must serialize at the shared-pool reservation
/// boundary. Without that reservation, both workers can observe an empty
/// map while the first turn owns the socket and open two sockets for one
/// conversation chain.
#[test]
fn shared_pool_serializes_same_key_turns() {
    let (addr, server) = spawn_fake_codex_server();
    let gate = Arc::new(ResponseGate::new());
    server.lock_state().response_gate = Some(Arc::clone(&gate));
    let config = Arc::new(make_config(
        &format!("http://{addr}/backend-api"),
        Some("acc"),
    ));
    let pool = Arc::new(SharedWsPool::new(Arc::new(crate::test_network_policy())));
    let first = {
        let config = config.clone();
        let pool = pool.clone();
        thread::spawn(move || run_shared_turn(&pool, &config, "same-session", "sp-0"))
    };
    gate.wait_for_arrival();

    let (waiting_tx, waiting_rx) = std_mpsc::sync_channel(1);
    pool.set_checkout_wait_hook(Arc::new(move || {
        waiting_tx.send(()).expect("checkout wait observer");
    }));
    let second = {
        let config = Arc::clone(&config);
        let pool = Arc::clone(&pool);
        thread::spawn(move || run_shared_turn(&pool, &config, "same-session", "sp-1"))
    };
    waiting_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("second turn reaches busy same-key condition wait");
    {
        let state = server.lock_state();
        assert_eq!(state.upgrade_count, 1);
        assert_eq!(state.requests.len(), 1);
        assert_eq!(state.active_turns, 1);
    }

    gate.release_one();
    gate.wait_for_arrival();
    gate.release_one();
    first.join().expect("first same-key turn");
    second.join().expect("second same-key turn");

    let state = server.lock_state();
    assert_eq!(
        state.upgrade_count, 1,
        "same PoolKey must reuse one reserved socket rather than opening a parallel chain"
    );
    assert_eq!(state.turns_per_connection, vec![2]);
}

/// A prompt canceled while it is queued behind a same-key WS reservation
/// must stop waiting instead of sending a stale network request after the
/// active turn releases. The pool registers prompt cancellation as a wake
/// source while parked on the condvar so the waiter can unwind and let the
/// worker emit its terminal canceled response/PromptDone.
#[test]
fn shared_pool_checkout_wait_aborts_when_canceled() {
    let config = make_config("https://chatgpt.com/backend-api", Some("acc"));
    let key = pool_key_for(
        &config,
        "test-agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    let pool = Arc::new(SharedWsPool::new(Arc::new(crate::test_network_policy())));
    pool.inner
        .lock()
        .expect("pool lock")
        .busy
        .insert(key.clone());

    let canceled = Arc::new(AtomicBool::new(false));
    let abort_waker = Arc::new(Mutex::new(None));
    let (registered_tx, registered_rx) = std_mpsc::channel();
    let (result_tx, result_rx) = std_mpsc::channel();
    let started = Arc::new(Barrier::new(2));
    let handle = {
        let pool = pool.clone();
        let key = key.clone();
        let canceled = canceled.clone();
        let abort_waker = Arc::clone(&abort_waker);
        let started = started.clone();
        thread::spawn(move || {
            let mut abort = AtomicAbort {
                canceled,
                waker: abort_waker,
                registered_tx,
            };
            started.wait();
            let result = pool.checkout_until(&key, "test", &mut abort);
            result_tx.send(result).expect("checkout result receiver");
        })
    };

    started.wait();
    registered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("abort waker registered");
    canceled.store(true, Ordering::SeqCst);
    let wake = abort_waker
        .lock()
        .expect("abort waker lock")
        .as_ref()
        .expect("registered abort waker")
        .clone();
    wake();

    let result = result_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("checkout abort result");
    handle.join().expect("checkout waiter join");
    assert!(matches!(result, Err(WsTurnError::Canceled)));
    assert!(
        pool.inner.lock().expect("pool lock").busy.contains(&key),
        "a canceled waiter must not steal or clear the active worker's reservation"
    );
}

/// A failed fresh upgrade must abandon its same-key reservation so a later
/// retry cannot remain parked behind work that no longer exists.
#[test]
fn failed_fresh_connect_releases_pool_reservation() {
    let pool = SharedWsPool::new(Arc::new(crate::test_network_policy()));
    let config = make_config("https://chatgpt.com/backend-api", Some("acc"));
    let key = pool_key_for(
        &config,
        "agent-connect-failure",
        tau_proto::PromptOriginator::User,
        false,
    );
    let mut abort = NeverAbort;
    assert!(
        pool.checkout_until(&key, &config.api_key, &mut abort)
            .expect("reserve fresh key")
            .is_none()
    );

    let network = crate::test_network_policy();
    let result = pool.connect_reserved_fresh(&key, &config, &mut abort, |_, _, _| {
        Err(LlmError::Outbound(network.deadline_error(
            "wss://target.example/codex/responses",
            tau_provider::OutboundPhase::Connect,
        )))
    });
    assert!(matches!(result, Err(WsTurnError::Other(_))));
    assert!(matches!(
        pool.try_checkout(&key, &config.api_key)
            .expect("reservation was released"),
        TryCheckout::Reserved(None)
    ));
    pool.abandon(&key).expect("clean test reservation");
}

/// Every cancellation boundary around a fresh connector must release the key:
/// before connect, from the connector, and after a socket was opened.
#[test]
fn canceled_fresh_connect_releases_pool_reservation_at_every_boundary() {
    struct ToggleAbort(bool);
    impl TurnAbort for ToggleAbort {
        fn is_aborted(&mut self) -> bool {
            self.0
        }

        fn register_waker(
            &mut self,
            _waker: Arc<dyn Fn() + Send + Sync + 'static>,
        ) -> Box<dyn TurnAbortWaker> {
            Box::new(TestAbortWaker)
        }
    }

    fn reserve(pool: &SharedWsPool, key: &PoolKey, config: &ResponsesConfig) {
        let mut never = NeverAbort;
        assert!(
            pool.checkout_until(key, &config.api_key, &mut never)
                .expect("reserve key")
                .is_none()
        );
    }

    fn assert_reusable(pool: &SharedWsPool, key: &PoolKey, config: &ResponsesConfig) {
        assert!(matches!(
            pool.try_checkout(key, &config.api_key)
                .expect("reservation reusable"),
            TryCheckout::Reserved(None)
        ));
        pool.abandon(key).expect("clean reusable reservation");
    }

    let pool = SharedWsPool::new(Arc::new(crate::test_network_policy()));
    let config = make_config("https://chatgpt.com/backend-api", Some("acc"));
    let key = pool_key_for(
        &config,
        "agent-connect-cancel",
        tau_proto::PromptOriginator::User,
        false,
    );

    reserve(&pool, &key, &config);
    let mut abort = ToggleAbort(true);
    let result = pool.connect_reserved_fresh(&key, &config, &mut abort, |_, _, _| {
        unreachable!("pre-canceled connect must not invoke connector")
    });
    assert!(matches!(result, Err(WsTurnError::Canceled)));
    assert_reusable(&pool, &key, &config);

    reserve(&pool, &key, &config);
    let mut abort = ToggleAbort(false);
    let result =
        pool.connect_reserved_fresh(&key, &config, &mut abort, |_, _, _| Err(LlmError::Canceled));
    assert!(matches!(result, Err(WsTurnError::Canceled)));
    assert_reusable(&pool, &key, &config);

    let (addr, _server) = spawn_fake_codex_server();
    let live_config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let live_key = pool_key_for(
        &live_config,
        "agent-post-connect-cancel",
        tau_proto::PromptOriginator::User,
        false,
    );
    reserve(&pool, &live_key, &live_config);
    let mut abort = ToggleAbort(false);
    let result = pool.connect_reserved_fresh(
        &live_key,
        &live_config,
        &mut abort,
        |config, thread, abort| {
            let mut never = NeverAbort;
            let conn = WsConn::connect(config, thread, &crate::test_network_policy(), &mut never)?;
            abort.0 = true;
            Ok(conn)
        },
    );
    assert!(matches!(result, Err(WsTurnError::Canceled)));
    assert_reusable(&pool, &live_key, &live_config);
}

/// Prewarm must never park behind an active same-key reservation. A busy key
/// means a real turn is already doing the warming work, so duplicate
/// best-effort work skips without creating another socket.
#[test]
fn shared_prewarm_skips_busy_same_key_without_waiting() {
    let (addr, _server) = spawn_fake_codex_server();
    let config = Arc::new(make_config(
        &format!("http://{addr}/backend-api"),
        Some("acc"),
    ));
    let pool = Arc::new(SharedWsPool::new(Arc::new(crate::test_network_policy())));
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

    let (tx, rx) = path_std_sync::mpsc::channel();
    let handle = {
        let config = config.clone();
        let pool = pool.clone();
        thread::spawn(move || {
            let session_id = tau_proto::SessionId::parse("same-session")
                .expect("known-safe SessionId must be valid");
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
            let mut abort = crate::NeverAbort;
            let result = run_prewarm_through_shared_pool(
                &pool,
                &config,
                "same-session",
                &request,
                &mut abort,
            );
            tx.send(result.is_ok()).expect("send prewarm result");
        })
    };

    let ok = match rx.recv_timeout(Duration::from_secs(2)) {
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
        pool.inner.lock().expect("pool lock").busy.contains(&key),
        "skipped prewarm must not clear the active worker's reservation"
    );
    assert_eq!(
        pool.stats().expect("pool stats").upgrades,
        0,
        "skipped prewarm should not open a socket"
    );
}

/// A silent peer after successful upgrade must remain cooperatively cancelable;
/// cancellation discards the socket and releases the exact pool reservation.
#[test]
fn shared_prewarm_silent_peer_cancels_and_releases_reservation() {
    let (addr, server) = spawn_fake_codex_server();
    server.lock_state().silent_response = true;
    let config = Arc::new(make_config(
        &format!("http://{addr}/backend-api"),
        Some("acc"),
    ));
    let pool = Arc::new(SharedWsPool::new(Arc::new(crate::test_network_policy())));
    let key = pool_key_for(
        &config,
        "test-agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    let canceled = Arc::new(AtomicBool::new(false));
    let waker = Arc::new(Mutex::new(None));
    let (registered_tx, registered_rx) = std_mpsc::channel();
    let worker = {
        let pool = Arc::clone(&pool);
        let config = Arc::clone(&config);
        let canceled = Arc::clone(&canceled);
        let waker = Arc::clone(&waker);
        thread::spawn(move || {
            let session_id = tau_proto::SessionId::parse("silent-prewarm")
                .expect("known-safe SessionId must be valid");
            let agent_id = tau_proto::AgentId::parse("test-agent").expect("agent id");
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
            let mut abort = AtomicAbort {
                canceled,
                waker,
                registered_tx,
            };
            run_prewarm_through_shared_pool(&pool, &config, "silent-prewarm", &request, &mut abort)
        })
    };
    registered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("connect cancellation registration");
    registered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("response cancellation registration");
    canceled.store(true, Ordering::SeqCst);
    waker
        .lock()
        .expect("abort waker")
        .as_ref()
        .expect("active response waker")();

    assert!(matches!(
        worker.join().expect("prewarm worker"),
        Err(LlmError::Canceled)
    ));
    assert!(
        matches!(
            pool.try_checkout(&key, &config.api_key)
                .expect("checkout after cancel"),
            TryCheckout::Reserved(None)
        ),
        "canceled prewarm must leave no stale socket"
    );
    pool.abandon(&key).expect("release test reservation");
}

/// Profile/session invalidation racing a reserved socket must prevent the old
/// owner from reinstalling it when its in-flight work later completes.
#[test]
fn invalidate_all_discards_late_reserved_socket_release() {
    let (addr, _server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let pool = SharedWsPool::new(Arc::new(crate::test_network_policy()));
    run_shared_turn(&pool, &config, "invalidate-session", "sp-warm");
    let key = pool_key_for(
        &config,
        "test-agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    let TryCheckout::Reserved(Some(conn)) = pool
        .try_checkout(&key, &config.api_key)
        .expect("reserve warm socket")
    else {
        panic!("expected reserved warm socket");
    };
    pool.invalidate_all().expect("invalidate pool");
    pool.release(key.clone(), conn).expect("late release");

    assert!(
        matches!(
            pool.try_checkout(&key, &config.api_key)
                .expect("checkout after invalidation"),
            TryCheckout::Reserved(None)
        ),
        "invalidated owner must not reinstall its stale socket"
    );
    pool.abandon(&key).expect("release test reservation");
}

/// Cancellation landing after socket installation but before reservation
/// publication must remove the staged socket before a waiter can reuse it.
#[test]
fn cancel_between_staged_release_and_finish_discards_socket() {
    let (addr, _server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let pool = SharedWsPool::new(Arc::new(crate::test_network_policy()));
    run_shared_turn(&pool, &config, "cancel-release", "sp-warm");
    let key = pool_key_for(
        &config,
        "test-agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    let TryCheckout::Reserved(Some(conn)) = pool
        .try_checkout(&key, &config.api_key)
        .expect("reserve warm socket")
    else {
        panic!("expected warm socket");
    };
    let generation = pool.claim_prewarm(&key).expect("claim prewarm");
    pool.stage_prewarm_release(&key, conn)
        .expect("stage prewarm release");
    pool.invalidate_prewarm(&key, generation)
        .expect("cancel reserved prewarm");
    pool.finish_prewarm_release(&key, generation)
        .expect("finish canceled release");

    assert!(matches!(
        pool.try_checkout(&key, &config.api_key)
            .expect("checkout after cancellation"),
        TryCheckout::Reserved(None)
    ));
    pool.abandon(&key).expect("release test reservation");
}

/// A staged socket stays busy while cancellation callback authority is being
/// unregistered, so a same-key waiter cannot take it before publication.
#[test]
fn staged_socket_is_not_checkoutable_before_guard_unregisters() {
    let (addr, _server) = spawn_fake_codex_server();
    let config = Arc::new(make_config(
        &format!("http://{addr}/backend-api"),
        Some("acc"),
    ));
    let pool = Arc::new(SharedWsPool::new(Arc::new(crate::test_network_policy())));
    run_shared_turn(&pool, &config, "publish-order", "sp-warm");
    let key = pool_key_for(
        &config,
        "test-agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    let TryCheckout::Reserved(Some(conn)) = pool
        .try_checkout(&key, &config.api_key)
        .expect("reserve warm socket")
    else {
        panic!("expected warm socket");
    };
    let generation = pool.claim_prewarm(&key).expect("claim prewarm");
    let (entered_tx, entered_rx) = std_mpsc::channel();
    let (release_tx, release_rx) = std_mpsc::channel();
    let publisher = {
        let pool = Arc::clone(&pool);
        let key = key.clone();
        thread::spawn(move || {
            let mut reservation = PrewarmReservation {
                pool: &pool,
                key,
                generation,
                armed: true,
            };
            reservation
                .publish(
                    conn,
                    Box::new(BlockingDropWaker {
                        entered: entered_tx,
                        release: release_rx,
                    }),
                    &mut NeverAbort,
                )
                .expect("publish socket");
        })
    };
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("publication reaches guard unregister");
    assert!(matches!(
        pool.try_checkout(&key, &config.api_key)
            .expect("checkout during unregister"),
        TryCheckout::Busy
    ));
    release_tx.send(()).expect("finish unregister");
    publisher.join().expect("publisher");
    assert!(matches!(
        pool.try_checkout(&key, &config.api_key)
            .expect("checkout after publication"),
        TryCheckout::Reserved(Some(_))
    ));
    pool.abandon(&key).expect("release test reservation");
}

/// Leaving prewarm work before normal release cannot strand the same-key busy
/// flag because scope-bound ownership abandons it.
#[test]
fn prewarm_reservation_drop_cleans_up_early_exit() {
    let config = make_config("https://example.invalid/backend-api", Some("acc"));
    let pool = SharedWsPool::new(Arc::new(crate::test_network_policy()));
    let key = pool_key_for(
        &config,
        "test-agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    assert!(matches!(
        pool.try_checkout(&key, &config.api_key)
            .expect("reserve prewarm"),
        TryCheckout::Reserved(None)
    ));
    {
        let _reservation = PrewarmReservation {
            pool: &pool,
            key: key.clone(),
            generation: pool.claim_prewarm(&key).expect("claim prewarm"),
            armed: true,
        };
    }
    assert!(matches!(
        pool.try_checkout(&key, &config.api_key)
            .expect("reserve after unwind"),
        TryCheckout::Reserved(None)
    ));
    pool.abandon(&key).expect("release test reservation");
}

/// The changed shared-pool prewarm path must install one socket that the next
/// real prompt reuses rather than opening a duplicate connection.
#[test]
fn shared_prewarm_socket_is_reused_by_real_turn() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let pool = SharedWsPool::new(Arc::new(crate::test_network_policy()));
    let session_id = tau_proto::SessionId::parse("shared-prewarm-reuse")
        .expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("test-agent").expect("agent id");
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
    let mut abort = NeverAbort;
    run_prewarm_through_shared_pool(&pool, &config, session_id.as_str(), &request, &mut abort)
        .expect("shared prewarm");
    run_shared_turn(&pool, &config, session_id.as_str(), "sp-after-prewarm");

    let state = server.lock_state();
    assert_eq!(state.upgrade_count, 1);
    assert_eq!(state.requests.len(), 2);
}

/// A failed production prewarm connect must abandon its reservation so later
/// work can retry the same cache owner.
#[test]
fn shared_prewarm_connect_failure_releases_reservation() {
    let config = make_config("file:///unsupported", Some("acc"));
    let pool = SharedWsPool::new(Arc::new(crate::test_network_policy()));
    let session_id =
        tau_proto::SessionId::parse("prewarm-failure").expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("test-agent").expect("agent id");
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
    let key = PoolKey::for_request(&config, &request);
    let mut abort = NeverAbort;
    assert!(
        run_prewarm_through_shared_pool(&pool, &config, session_id.as_str(), &request, &mut abort,)
            .is_err()
    );
    assert!(matches!(
        pool.try_checkout(&key, &config.api_key)
            .expect("reservation after failure"),
        TryCheckout::Reserved(None)
    ));
    pool.abandon(&key).expect("release test reservation");
}

/// Cancellation visible before a cached prewarm starts must prevent any
/// `response.create` send and leave the key available for later work.
#[test]
fn already_canceled_cached_prewarm_sends_no_request() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let pool = SharedWsPool::new(Arc::new(crate::test_network_policy()));
    run_shared_turn(&pool, &config, "already-canceled", "sp-warm");
    let session_id = tau_proto::SessionId::parse("already-canceled")
        .expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("test-agent").expect("agent id");
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
    let key = PoolKey::for_request(&config, &request);
    let (registered_tx, _registered_rx) = std_mpsc::channel();
    let mut abort = AtomicAbort {
        canceled: Arc::new(AtomicBool::new(true)),
        waker: Arc::new(Mutex::new(None)),
        registered_tx,
    };
    assert!(matches!(
        run_prewarm_through_shared_pool(&pool, &config, session_id.as_str(), &request, &mut abort,),
        Err(LlmError::Canceled)
    ));
    assert_eq!(
        server.lock_state().requests.len(),
        1,
        "canceled prewarm must not send after the warm-up turn"
    );
    assert!(matches!(
        pool.try_checkout(&key, &config.api_key)
            .expect("reservation after cancellation"),
        TryCheckout::Reserved(None)
    ));
    pool.abandon(&key).expect("release test reservation");
}

/// Dropping ownership after staging must remove both the staged socket and busy
/// reservation if normal publication cannot finish.
#[test]
fn staged_prewarm_reservation_drop_removes_socket() {
    let (addr, _server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let pool = SharedWsPool::new(Arc::new(crate::test_network_policy()));
    run_shared_turn(&pool, &config, "staged-drop", "sp-warm");
    let key = pool_key_for(
        &config,
        "test-agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    let TryCheckout::Reserved(Some(conn)) = pool
        .try_checkout(&key, &config.api_key)
        .expect("reserve warm socket")
    else {
        panic!("expected warm socket");
    };
    let generation = pool.claim_prewarm(&key).expect("claim prewarm");
    let reservation = PrewarmReservation {
        pool: &pool,
        key: key.clone(),
        generation,
        armed: true,
    };
    pool.stage_prewarm_release(&key, conn)
        .expect("stage release");
    drop(reservation);
    assert!(matches!(
        pool.try_checkout(&key, &config.api_key)
            .expect("reservation after staged drop"),
        TryCheckout::Reserved(None)
    ));
    pool.abandon(&key).expect("release test reservation");
}

/// Different prompt-cache thread keys should not be serialized by the same-key
/// guard. The shared mutex may protect bookkeeping, but it must not cover
/// network I/O.
#[test]
fn shared_pool_allows_different_keys_to_run_concurrently() {
    let (addr, server) = spawn_fake_codex_server();
    let gate = Arc::new(ResponseGate::new());
    server.lock_state().response_gate = Some(Arc::clone(&gate));
    let config = Arc::new(make_config(
        &format!("http://{addr}/backend-api"),
        Some("acc"),
    ));
    let pool = Arc::new(SharedWsPool::new(Arc::new(crate::test_network_policy())));
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
    gate.wait_for_arrival();
    gate.wait_for_arrival();
    gate.release_one();
    gate.release_one();
    for handle in handles {
        handle.join().expect("worker join");
    }

    let state = server.lock_state();
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
    assert_eq!(server.lock_state().upgrade_count, 3);

    // Touching A again must re-upgrade (its old socket got
    // evicted on C's release).
    run_turn_for_agent(&mut pool, &config, "session-lru", "agent-a", &mut on_update);
    assert_eq!(server.lock_state().upgrade_count, 4);
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
    assert_eq!(server.lock_state().upgrade_count, 1);

    // Forcibly age the cached connection past the threshold.
    let key = pool_key_for(
        &config,
        "test-agent",
        tau_proto::PromptOriginator::User,
        false,
    );
    if let Some(conn) = pool.conns.get_mut(&key) {
        conn.opened_at =
            path_std_time::Instant::now() - MAX_CONNECTION_AGE - Duration::from_secs(1);
    } else {
        panic!("expected connection in pool");
    }

    // Next turn must reopen rather than send on the stale socket.
    run_turn(&mut pool, &config, "session-aged", &mut on_update);
    assert_eq!(
        server.lock_state().upgrade_count,
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

    let session_id =
        tau_proto::SessionId::parse("session-x").expect("known-safe SessionId must be valid");
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

/// Regression for Clank k2hu: input accepted while a response is in flight
/// precedes that response canonically, but is absent from its upstream history.
/// The next same-socket turn must full-replay both inputs, then re-anchor the
/// successful replay so a later compatible continuation can send only its
/// suffix.
#[test]
fn response_anchor_replays_async_input_before_in_flight_response() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let response_gate = Arc::new(ResponseGate::new());
    server.lock_state().response_gate = Some(Arc::clone(&response_gate));

    let initial = user_msg("initial context");
    let first_context = context(std::slice::from_ref(&initial));
    let first_config = config.clone();
    let first_turn = thread::spawn(move || {
        let mut pool = WsPool::new();
        let state = run_context_turn(
            &mut pool,
            &first_config,
            "session-causal-anchor",
            "sp-first",
            first_context,
        );
        (pool, state)
    });

    response_gate.wait_for_arrival();
    let async_inputs = vec![
        user_msg("reviewer architecture: PASS"),
        user_msg("reviewer reliability: PASS"),
    ];
    response_gate.release_one();
    let (mut pool, first_state) = first_turn.join().expect("join first turn");
    server.lock_state().response_gate = None;

    let first_response_id = first_state.response_id.clone().expect("first response id");
    let first_output = first_state.into_output_items();
    let second_context = block_context(vec![
        tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
            items: vec![initial.clone()],
        }),
        tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
            items: async_inputs.clone(),
        }),
        tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
            provider_response_id: Some(first_response_id),
            backend: None,
            output_items: first_output,
            usage: None,
        }),
    ]);
    let second_state = run_context_turn(
        &mut pool,
        &config,
        "session-causal-anchor",
        "sp-second",
        second_context,
    );

    {
        let state = server.lock_state();
        assert_eq!(state.upgrade_count, 1, "both turns must share one socket");
        let request = &state.requests[1];
        assert!(
            request.get("previous_response_id").is_none(),
            "causally incompatible response id must not anchor a delta",
        );
        let input = request["input"].as_array().expect("full replay input");
        assert_eq!(input.len(), 4, "C, both async inputs, and R must replay");
        assert_eq!(
            input[1]["content"][0]["text"],
            "reviewer architecture: PASS"
        );
        assert_eq!(input[2]["content"][0]["text"], "reviewer reliability: PASS");
    }

    let second_response_id = second_state
        .response_id
        .clone()
        .expect("second response id");
    let second_output = second_state.into_output_items();
    let mut third_blocks = second_context.blocks.clone();
    third_blocks.push(tau_proto::ContextBlock::AssistantResponse(
        tau_proto::AssistantResponseBlock {
            provider_response_id: Some(second_response_id.clone()),
            backend: None,
            output_items: second_output,
            usage: None,
        },
    ));
    third_blocks.push(tau_proto::ContextBlock::UserInput(
        tau_proto::UserInputBlock {
            items: vec![user_msg("continue after reviews")],
        },
    ));
    run_context_turn(
        &mut pool,
        &config,
        "session-causal-anchor",
        "sp-third",
        block_context(third_blocks),
    );

    let state = server.lock_state();
    let request = &state.requests[2];
    assert_eq!(
        request["previous_response_id"], second_response_id,
        "successful full replay must publish a new compatible anchor",
    );
    let input = request["input"].as_array().expect("delta input");
    assert_eq!(input.len(), 1, "re-anchored turn must send only its suffix");
    assert_eq!(input[0]["content"][0]["text"], "continue after reviews");
}

/// Two ordinary turns shaped by V1 placement send `H` first, then use the
/// response anchor to send the exact once-only `Q` suffix from `H, R, Q`.
#[test]
fn response_anchor_keeps_compatible_incremental_reuse() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    let initial = user_msg("H");
    let first_state = run_context_turn(
        &mut pool,
        &config,
        "session-compatible-anchor",
        "sp-first",
        context(std::slice::from_ref(&initial)),
    );
    let response_id = first_state.response_id.clone().expect("response id");
    let second_context = block_context(vec![
        tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
            items: vec![initial],
        }),
        tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
            provider_response_id: Some(response_id.clone()),
            backend: None,
            output_items: first_state.into_output_items(),
            usage: None,
        }),
        tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
            items: vec![user_msg("Q")],
        }),
    ]);
    run_context_turn(
        &mut pool,
        &config,
        "session-compatible-anchor",
        "sp-second",
        second_context,
    );

    let state = server.lock_state();
    assert_eq!(state.upgrade_count, 1, "both turns must share one socket");
    let first_request = &state.requests[0];
    assert!(
        first_request.get("previous_response_id").is_none(),
        "the first request has no response anchor"
    );
    let first_input = first_request["input"].as_array().expect("first input");
    assert_eq!(first_input.len(), 1);
    assert_eq!(first_input[0]["content"][0]["text"], "H");
    let request = &state.requests[1];
    assert_eq!(request["previous_response_id"], response_id);
    let input = request["input"].as_array().expect("delta input");
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["content"][0]["text"], "Q");
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

    let session_id =
        tau_proto::SessionId::parse("session-headers").expect("known-safe SessionId must be valid");
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
    let expected = request.prompt_cache_key(&config.base_url, config.mode);

    run_turn_through_pool(
        &mut pool,
        &config,
        "session-headers",
        "sp-test",
        &request,
        &mut on_update,
    )
    .expect("turn ok");

    let s = server.lock_state();
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

/// A successful `generate:false` prewarm anchors an exact compatible prefix on
/// the same socket, so the real turn sends only its suffix.
#[test]
fn prewarm_chains_exact_prefix_on_same_socket() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let session_id =
        tau_proto::SessionId::parse("session-prewarm").expect("known-safe SessionId must be valid");
    let prewarmed_messages = vec![user_msg("AGENTS.md context")];
    let real_messages = vec![user_msg("AGENTS.md context"), user_msg("actual request")];
    let agent_id = tau_proto::AgentId::parse("test-agent").expect("agent id");
    let prior = PromptPayload {
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
        "session-prewarm",
        "sp-prior",
        &prior,
        &mut |_| {},
    )
    .expect("prior real turn succeeds");

    let prewarm = PromptPayload {
        system_prompt: "sys",
        context: context(&prewarmed_messages),
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

    let s = server.lock_state();
    assert_eq!(s.upgrade_count, 1, "prewarm and turn must share one socket");
    assert_eq!(
        s.requests.len(),
        3,
        "expected prior, prewarm, and real turn"
    );
    let warm = &s.requests[1];
    let turn = &s.requests[2];
    assert_eq!(
        warm.get("generate").and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        turn.get("previous_response_id"),
        Some(&serde_json::Value::String("resp_0_2".to_owned())),
        "compatible same-socket prewarm must supersede the older real-turn chain",
    );
    assert_eq!(
        turn.get("input")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(1),
        "real turn should send only the suffix after the warmed prefix",
    );
}

/// A prewarm anchor is valid only for the exact non-input request fingerprint.
/// A changed instruction set must discard the anchor and replay full context.
#[test]
fn prewarm_fingerprint_divergence_discards_chain_anchor() {
    let (addr, server) = spawn_fake_codex_server();
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    let session_id = tau_proto::SessionId::parse("session-prewarm-divergence")
        .expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("test-agent").expect("agent id");
    let originator = tau_proto::PromptOriginator::User;
    let prefix = vec![user_msg("stable prefix")];
    let prewarm = PromptPayload {
        system_prompt: "old instructions",
        context: context(&prefix),
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
    run_prewarm_through_pool(&mut pool, &config, session_id.as_str(), &prewarm)
        .expect("prewarm succeeds");

    let full = vec![user_msg("stable prefix"), user_msg("real suffix")];
    let divergent = PromptPayload {
        system_prompt: "new instructions",
        context: context(&full),
        ..prewarm
    };
    run_turn_through_pool(
        &mut pool,
        &config,
        session_id.as_str(),
        "sp-divergent",
        &divergent,
        &mut |_| {},
    )
    .expect("divergent turn succeeds with full replay");

    {
        let state = server.lock_state();
        let turn = &state.requests[1];
        assert!(turn.get("previous_response_id").is_none());
        assert_eq!(
            turn["input"].as_array().map(Vec::len),
            Some(2),
            "divergent fingerprint must send full input",
        );
    }
    let compatible_after_mismatch = PromptPayload {
        system_prompt: "old instructions",
        context: context(&full),
        ..prewarm
    };
    run_turn_through_pool(
        &mut pool,
        &config,
        session_id.as_str(),
        "sp-compatible-after-mismatch",
        &compatible_after_mismatch,
        &mut |_| {},
    )
    .expect("compatible turn after mismatch succeeds");
    let state = server.lock_state();
    let turn = &state.requests[2];
    assert!(
        turn.get("previous_response_id").is_none(),
        "the divergent turn must consume the prewarm anchor"
    );
    assert_eq!(
        turn["input"].as_array().map(Vec::len),
        Some(2),
        "consumed anchor cannot be reused by a later compatible prompt",
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

    let session_id =
        tau_proto::SessionId::parse("session-fresh").expect("known-safe SessionId must be valid");
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

    let s = server.lock_state();
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
    let session_id = tau_proto::SessionId::parse("session-compacted")
        .expect("known-safe SessionId must be valid");
    let messages = vec![
        tau_proto::ContextItem::Compaction(tau_proto::OpaqueProviderItem::new(
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

    let s = server.lock_state();
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

/// A cached connection dies mid-turn (WebSocket control-ping timeout / TCP
/// reset). If the request has a `previous_response_id`, the pool must reopen a
/// fresh WS socket, strip the stale chain id, and leave the replacement socket
/// in the pool so later turns regain cache warmth.
#[test]
fn mid_stream_close_with_chain_rebuilds_ws_warmth() {
    let (addr, server) = spawn_fake_codex_server();
    // Make connection #0 die mid-turn-#2 (after_turn=1 -> the
    // second arriving turn on conn 0 is the one that gets closed).
    server.lock_state().fault = Some(MidStreamCloseFault {
        on_conn_index: 0,
        after_turn: 1,
    });
    let mut config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    config.mode = ResponsesMode::LiteCompatibility;
    let mut pool = WsPool::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    // Turn 1: opens conn-0, returns a `response_id` the harness
    // would chain off for turn 2.
    let session_id =
        tau_proto::SessionId::parse("session-die").expect("known-safe SessionId must be valid");
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
    let prev_id = state1
        .response_id
        .clone()
        .expect("first turn yielded response_id");
    let first_output = state1.into_output_items();

    // Turn 2: harness wants to chain via `prev_id`. The cached socket dies
    // mid-stream; pool must reopen cold WS and strip the chain id rather
    // than sticky-disabling WS for the session.
    let req2 = PromptPayload {
        system_prompt: "sys",
        context: context_after_response(&prev_id, first_output, vec![user_msg("second turn")]),
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

    let s = server.lock_state();
    assert_eq!(
        s.upgrade_count, 2,
        "mid-stream close should force one replacement WS upgrade"
    );
    // Three captured requests in arrival order:
    //   #0: turn-1 on conn-0 (no chain id, no prior response)
    //   #1: turn-2 on conn-0 (had chain id; this is the one that died)
    //   #2: turn-2 replay on conn-1 (chain stripped for fresh WS)
    assert_eq!(s.requests.len(), 3, "expected one WS replay envelope");
    assert!(s.requests.iter().all(|request| {
        request
            .pointer("/client_metadata/ws_request_header_x_openai_internal_codex_responses_lite")
            .and_then(serde_json::Value::as_str)
            == Some("true")
            && request["parallel_tool_calls"] == false
    }));
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
    server.lock_state().fault = Some(MidStreamCloseFault {
        on_conn_index: 0,
        after_turn: 1,
    });
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let pool = SharedWsPool::new(Arc::new(crate::test_network_policy()));
    let mut connecting_count = 0;
    let mut dispatched_count = 0;
    let mut last_response_bytes = 0;
    let mut response_bytes_monotonic = true;
    let mut on_update = |update: crate::StreamUpdate<'_>| {
        if matches!(update, crate::StreamUpdate::Connecting) {
            connecting_count += 1;
        }
        if matches!(update, crate::StreamUpdate::Dispatched(_)) {
            dispatched_count += 1;
            last_response_bytes = 0;
        }
        if let crate::StreamUpdate::Response(state) = update {
            let current = state.response_bytes_received();
            response_bytes_monotonic &= last_response_bytes <= current;
            last_response_bytes = current;
        }
    };

    let session_id = tau_proto::SessionId::parse("session-shared-die")
        .expect("known-safe SessionId must be valid");
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
    let mut abort = NeverAbort;
    let state1 = run_turn_through_shared_pool(
        &pool,
        &config,
        "sp-shared-1",
        &req1,
        None,
        &mut abort,
        &mut on_update,
    )
    .expect("first shared turn ok");
    let prev_id = state1
        .response_id
        .clone()
        .expect("first turn yielded response_id");
    let first_output = state1.into_output_items();

    let req2 = PromptPayload {
        system_prompt: "sys",
        context: context_after_response(&prev_id, first_output, vec![user_msg("second turn")]),
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
    let mut abort = NeverAbort;
    run_turn_through_shared_pool(
        &pool,
        &config,
        "sp-shared-2",
        &req2,
        None,
        &mut abort,
        &mut on_update,
    )
    .expect("shared chained reconnect should rebuild WS warmth");

    let s = server.lock_state();
    assert_eq!(
        s.upgrade_count, 2,
        "shared reconnect should open exactly one replacement socket"
    );
    assert_eq!(
        connecting_count, 2,
        "initial and replacement fresh connections each report connecting"
    );
    assert_eq!(
        dispatched_count, 2,
        "two logical turns emit exactly two dispatch origins despite transparent repair"
    );
    assert!(
        response_bytes_monotonic,
        "transport bytes remain cumulative across the discarded cached attempt"
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
/// missed `"ws writer task gone"` and `"websocket_control_ping failed:
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
        "stream error: websocket_control_ping failed: IO error: broken pipe",
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

/// The absolute prewarm deadline owns the whole cache-only attempt and must not
/// trigger the cached-socket reopen path for another response wait.
#[test]
fn prewarm_absolute_timeout_is_not_recoverable() {
    assert!(!is_recoverable_ws_error(&LlmError::HttpStatus(
        0,
        "websocket prewarm response timeout".to_owned(),
    )));
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

/// Idle watchdog failures are local terminal turn errors, not evidence that a
/// cached WebSocket is stale. The pool must not silently reconnect and replay a
/// timed-out turn, because that would extend a stuck prompt by another full
/// idle watchdog window.
#[test]
fn provider_stream_idle_timeout_is_not_silent_reconnect() {
    let err = LlmError::HttpStatus(
        0,
        "stream error: provider stream idle timeout: transport=Websocket".to_owned(),
    );
    assert!(!is_recoverable_ws_error(&err));
}

/// The exact upstream connection-cap code takes precedence over the generic
/// stream-error repair rule and must not consume a replacement connection.
#[test]
fn connection_limit_code_precedes_generic_stream_recovery() {
    let error = LlmError::StreamError {
        body: "stream error: capacity (type=websocket_connection_limit_reached)".to_owned(),
        code: Some("websocket_connection_limit_reached".to_owned()),
        retry_after: None,
    };
    assert!(is_recoverable_ws_error(&error));
}

/// Canonical stale-chain codes are distinguished from generic transport loss so
/// a successful full replay records the approved stale-chain disposition.
#[test]
fn canonical_stale_chain_code_is_typed_before_recovery() {
    let error = LlmError::StreamError {
        body: "stream error: rejected (type=previous_response_not_found)".to_owned(),
        code: Some("previous_response_not_found".to_owned()),
        retry_after: None,
    };
    assert!(is_stale_chain_error(&error));
    assert_eq!(recovery_decision(&error, false), RecoveryDecision::Repair);
}

/// Provider prose cannot impersonate a canonical recovery code.
#[test]
fn recovery_precedence_ignores_reserved_markers_in_prose() {
    let error = LlmError::StreamError {
        body: "stream error: echoed (type=previous_response_not_found) \
               (type=transport_error)"
            .to_owned(),
        code: Some("transport_error".to_owned()),
        retry_after: None,
    };
    assert!(!is_stale_chain_error(&error));
}

/// Once semantic output exists, even an otherwise repairable dead socket must
/// surface to the extension so tentative output can be cleared before retry.
#[test]
fn semantic_progress_exhausts_internal_recovery_budget() {
    let error = LlmError::HttpStatus(0, "stream error: ws closed".to_owned());
    assert_eq!(recovery_decision(&error, true), RecoveryDecision::Surface);
    assert_eq!(recovery_decision(&error, false), RecoveryDecision::Repair);
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
    /// Optional explicit request/release synchronization for concurrency tests.
    response_gate: Option<Arc<ResponseGate>>,
    /// Whether this peer keeps the turn silent until the client disconnects.
    silent_response: bool,
    /// Number of fake server turns currently awaiting or emitting a response.
    active_turns: usize,
    /// Maximum simultaneous fake server turns observed during a test.
    max_active_turns: usize,
    /// Fault injection. When `Some`, the worker for a matching
    /// connection drops the socket with a 1011 close frame
    /// instead of serving the offending turn — mimicking the
    /// WebSocket control-ping timeout the live Codex server produces
    /// when its idle reaper fires. Tests use this to exercise
    /// the silent-reconnect path.
    fault: Option<MidStreamCloseFault>,
    /// Optional provider error envelope emitted instead of the normal success.
    scripted_error: Option<serde_json::Value>,
}

/// Explicit request-arrival and response-release synchronization for tests.
struct ResponseGate {
    /// Reports each server request that reached the response boundary.
    arrival_tx: std_mpsc::Sender<()>,
    /// Receives request-arrival acknowledgements.
    arrival_rx: Mutex<std_mpsc::Receiver<()>>,
    /// Grants one server turn permission to emit its response.
    release_tx: std_mpsc::Sender<()>,
    /// Receives one release permit per server turn.
    release_rx: Mutex<std_mpsc::Receiver<()>>,
}

impl ResponseGate {
    /// Creates an empty response gate.
    fn new() -> Self {
        let (arrival_tx, arrival_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        Self {
            arrival_tx,
            arrival_rx: Mutex::new(arrival_rx),
            release_tx,
            release_rx: Mutex::new(release_rx),
        }
    }

    /// Waits for one request to reach the server response boundary.
    fn wait_for_arrival(&self) {
        self.arrival_rx
            .lock()
            .expect("arrival receiver")
            .recv_timeout(Duration::from_secs(1))
            .expect("server request arrival");
    }

    /// Allows one waiting server turn to emit its response.
    fn release_one(&self) {
        self.release_tx.send(()).expect("response release receiver");
    }

    /// Reports arrival and waits for one explicit response permit.
    fn arrive_and_wait(&self) {
        self.arrival_tx.send(()).expect("arrival observer");
        self.release_rx
            .lock()
            .expect("release receiver")
            .recv_timeout(Duration::from_secs(5))
            .expect("response release");
    }
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

/// Joined loopback WebSocket server with finite connection and request bounds.
struct FakeCodexServer {
    /// Listener address used to wake blocking accept during teardown.
    addr: SocketAddr,
    /// Captured server state used by assertions.
    state: Arc<Mutex<ServerState>>,
    /// Signals the accept loop to stop.
    shutdown: Arc<AtomicBool>,
    /// Joined accept supervisor, which in turn joins connection workers.
    supervisor: Option<thread::JoinHandle<()>>,
}

impl FakeCodexServer {
    /// Locks captured server state for test setup or assertions.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, ServerState> {
        self.state.lock().expect("fake Codex server state")
    }
}

impl Drop for FakeCodexServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(supervisor) = self.supervisor.take() {
            let result = supervisor.join();
            if !thread::panicking() {
                result.expect("join fake Codex server");
            }
        }
    }
}

fn spawn_fake_codex_server() -> (SocketAddr, FakeCodexServer) {
    const MAX_CONNECTIONS: usize = 32;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let state = Arc::new(Mutex::new(ServerState::default()));
    let state_clone = state.clone();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let supervisor = thread::spawn(move || {
        let mut workers = Vec::new();
        for connection_index in 0..MAX_CONNECTIONS {
            let (stream, _) = listener.accept().expect("accept fake Codex connection");
            if worker_shutdown.load(Ordering::SeqCst) {
                break;
            }
            let conn_state = state_clone.clone();
            workers.push(thread::spawn(move || {
                handle_one_connection(stream, conn_state);
            }));
            assert!(
                connection_index + 1 < MAX_CONNECTIONS,
                "fake Codex connection bound exhausted"
            );
        }
        for worker in workers {
            worker.join().expect("join fake Codex connection");
        }
    });
    (
        addr,
        FakeCodexServer {
            addr,
            state,
            shutdown,
            supervisor: Some(supervisor),
        },
    )
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
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("bound fake Codex reads");
    let mut upgrade_headers = BTreeMap::new();
    let mut ws = match tungstenite::accept_hdr(
        stream,
        #[allow(clippy::result_large_err)]
        |request: &path_tungstenite_handshake::server::Request, response| {
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
                let (fault_now, response_gate, silent_response, scripted_error) = {
                    let mut s = state.lock().expect("server state lock");
                    assert!(s.requests.len() < 128, "fake Codex request bound");
                    s.requests.push(parsed.clone());
                    s.turns_per_connection[conn_idx] += 1;
                    s.active_turns += 1;
                    s.max_active_turns = s.max_active_turns.max(s.active_turns);
                    let fault_now = s
                        .fault
                        .filter(|f| f.on_conn_index == conn_idx && turn_counter >= f.after_turn);
                    (
                        fault_now,
                        s.response_gate.clone(),
                        s.silent_response,
                        s.scripted_error.clone(),
                    )
                };
                turn_counter += 1;
                if let Some(gate) = response_gate {
                    gate.arrive_and_wait();
                }
                if fault_now.is_some() {
                    // Mimic the live Codex 1011 WebSocket-control-ping timeout
                    // drop: send a close frame and bail without
                    // streaming the response body. Client side
                    // sees `Message::Close` → `LlmError(0, "stream
                    // error: ws closed mid-stream ...")`.
                    let _ = ws.send(Message::Close(Some(tungstenite::protocol::CloseFrame {
                        code: path_tungstenite_protocol_frame_coding::CloseCode::Error,
                        reason: "keepalive ping timeout".into(),
                    })));
                    finish_server_turn(&state);
                    return;
                }
                if silent_response {
                    while let Ok(message) = ws.read() {
                        if matches!(message, Message::Close(_)) {
                            break;
                        }
                    }
                    finish_server_turn(&state);
                    return;
                }
                if let Some(error) = scripted_error {
                    ws.send(Message::Text(error.to_string().into()))
                        .expect("write scripted provider error");
                    finish_server_turn(&state);
                    continue;
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
    let session_id =
        tau_proto::SessionId::parse("test-session").expect("known-safe SessionId must be valid");
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
        responses_raw_json: None,
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

fn run_context_turn(
    pool: &mut WsPool,
    config: &ResponsesConfig,
    session: &str,
    agent_prompt_id: &str,
    prompt_context: &'static tau_proto::PromptContext,
) -> crate::common::StreamState {
    let session_id =
        tau_proto::SessionId::parse(session).expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("test-agent").expect("agent id");
    let request = PromptPayload {
        system_prompt: "sys",
        context: prompt_context,
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
        pool,
        config,
        session,
        agent_prompt_id,
        &request,
        &mut |_| {},
    )
    .expect("turn ok")
}

fn run_turn_for_agent(
    pool: &mut WsPool,
    config: &ResponsesConfig,
    session: &str,
    agent: &str,
    on_update: &mut impl FnMut(&crate::common::StreamState),
) {
    let session_id =
        tau_proto::SessionId::parse(session).expect("known-safe SessionId must be valid");
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
    let mut abort = NeverAbort;
    run_shared_turn_with_abort(pool, config, session, agent, agent_prompt_id, &mut abort);
}

/// Runs one shared-pool turn with an observable abort source.
fn run_shared_turn_with_abort(
    pool: &SharedWsPool,
    config: &ResponsesConfig,
    session: &str,
    agent: &str,
    agent_prompt_id: &str,
    abort: &mut impl TurnAbort,
) {
    let session_id =
        tau_proto::SessionId::parse(session).expect("known-safe SessionId must be valid");
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
    let mut on_update = |_: crate::StreamUpdate<'_>| {};
    run_turn_through_shared_pool(
        pool,
        config,
        agent_prompt_id,
        &request,
        None,
        abort,
        &mut on_update,
    )
    .expect("shared turn ok");
}

fn make_config(base_url: &str, account_id: Option<&str>) -> ResponsesConfig {
    ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        base_url: base_url.into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        raw_context_window: 258400,
        account_id: account_id.map(str::to_owned),
        supports_reasoning_effort: false,
        supports_reasoning_summary: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_compaction: false,
        supports_prompt_cache_key: false,
        supports_encrypted_reasoning: false,
    }
}

/// Opposite startup-selected Responses modes derive distinct pool keys even
/// for the same account, endpoint, and durable agent.
#[test]
fn pool_key_separates_responses_modes() {
    let mut standard = make_config("https://chatgpt.com/backend-api", Some("acct"));
    let mut lite = standard.clone();
    standard.mode = ResponsesMode::Standard;
    lite.mode = ResponsesMode::LiteCompatibility;
    let session_id =
        tau_proto::SessionId::parse("mode-test").expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("mode-agent").expect("agent id");
    let prompt_context = context(&[]);
    let request = PromptPayload {
        system_prompt: "sys",
        context: prompt_context,
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

    assert_ne!(
        PoolKey::for_request(&standard, &request),
        PoolKey::for_request(&lite, &request)
    );
}
/// A provider-authored context rejection must never enter the cached-socket
/// silent reconnect path that replays the full logical request.
#[test]
fn context_window_rejection_does_not_trigger_full_request_replay() {
    let error = LlmError::ProviderFailure(
        tau_proto::ProviderFailureKind::ContextWindowExceeded,
        "stream error: maximum context reached (type=context_length_exceeded)".to_owned(),
    );
    assert!(!is_recoverable_ws_error(&error));
}
