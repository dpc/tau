//! WebSocket connection pool for the Codex Responses backend.
//!
//! See `TODO-codex-websocket.md` §2 for the design rationale. Recap:
//!
//! - The provider processes prompts concurrently, so it can alternate between
//!   conversations (different sessions, sub-agent delegations interleaved with
//!   the parent). The OpenAI WS endpoint only caches the *most recent*
//!   `previous_response_id` per socket, so routing A → B → A on one shared
//!   socket would flush each chain's warmth on every switch. Keep one
//!   connection per upstream prompt-cache/thread UUID so warmth survives
//!   context-switches.
//! - Connection-in-flight exclusivity is enforced by ownership plus the shared
//!   wrapper's per-key busy set: checkout removes the connection from the map
//!   before a worker runs the turn, and same-key workers wait until release or
//!   drop before retrying. Different keys do not wait on each other's network
//!   turns.
//! - Bounded by a soft cap (env-tunable `TAU_WS_POOL_MAX`,
//!   [`DEFAULT_POOL_MAX`]). LRU eviction when full.
//! - Connections age out near the server's 60-minute hard cap so a call doesn't
//!   fail mid-turn from the server slamming the door.
//! - Bearer-mismatch on checkout means OAuth refreshed; drop the stale socket
//!   and open a new one.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use lru::LruCache;

use super::ResponsesConfig;
use super::ws::WsConn;
#[cfg(test)]
use crate::NeverAbort;
use crate::TurnAbort;
use crate::common::{LlmError, PromptPayload};

/// Default soft cap on simultaneously-cached WS connections.
///
/// One per `(account, prompt-cache/thread UUID)`. A typical interactive
/// workload runs 1–3 active sessions/conversations (the user's main + any
/// in-flight sub-agent delegation). The cap exists to bound pathological growth
/// (a long-lived agent process where the user reopens many old
/// sessions), not because the normal path needs many slots.
pub const DEFAULT_POOL_MAX: usize = 10;

/// Environment variable that overrides [`DEFAULT_POOL_MAX`] at
/// `WsPool::new()` time.
pub const POOL_MAX_ENV: &str = "TAU_WS_POOL_MAX";

/// Margin under the server's 60-minute hard cap before we
/// pre-emptively reopen a connection on checkout. Five minutes is
/// safer than cutting it close — a 59-minute-old connection that
/// dies *after* we send `response.create` surfaces as a mid-stream
/// `stream error` to the user, which a `<55min ? reuse : reopen`
/// check avoids entirely.
pub const MAX_CONNECTION_AGE: Duration = Duration::from_secs(55 * 60);

/// Pool key. A connection caches the previous_response of one
/// conversation chain; different chains get different sockets so
/// alternating between them preserves each chain's warm cache.
///
/// - `base_url` + `account_id` form a "socket realm" — same bearer, same
///   server-side state. Cross-realm reuse is impossible.
/// - `thread_id` is the upstream session/thread UUID sent on the WebSocket
///   upgrade. It is the same UUID as the request body's `prompt_cache_key`, so
///   a socket is never reused for a different cache bucket.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PoolKey {
    /// WS endpoint realm. Cross-realm reuse is impossible.
    pub base_url: String,
    /// Account realm for the socket's bearer and server-side state.
    pub account_id: Option<String>,
    /// Upstream ChatGPT/Codex session/thread UUID for this socket.
    ///
    /// This is derived from the same value as the request body's
    /// `prompt_cache_key` and is sent as both `session-id` and `thread-id` on
    /// the WebSocket upgrade.
    pub thread_id: String,
}

impl PoolKey {
    /// Build the socket key for a prompt request.
    ///
    /// The request's prompt-cache UUID becomes the upstream `thread-id` and
    /// `session-id` headers, so it is part of the pool key rather than a
    /// per-turn detail.
    pub fn for_request(config: &ResponsesConfig, request: &PromptPayload<'_>) -> Self {
        Self {
            base_url: config.base_url.clone(),
            account_id: config.account_id.clone(),
            thread_id: request.prompt_cache_key(&config.base_url),
        }
    }
}

/// Single-threaded pool of WS connections.
///
/// Hot path (turn N+1 on a known thread): `checkout` returns the
/// existing `WsConn` (removed from the map); the caller runs the
/// turn; on success it calls `release` to put the conn back at the
/// head of the LRU queue. On error (mid-stream close, IO break),
/// the caller drops the connection — the entry is already removed
/// from the map and the LRU list resyncs lazily.
pub struct WsPool {
    conns: LruCache<PoolKey, WsConn>,
    stats: WsPoolStats,
}

/// Lifetime counters for the WS pool. Bumped on each interesting
/// path so an operator can grep provider tracing output and see
/// how often the silent-reconnect machinery kicked in (or, more
/// importantly, *kept* kicking in for a thread — a runaway count
/// is the signature of an upstream regression).
#[derive(Clone, Copy, Debug, Default)]
pub struct WsPoolStats {
    /// Fresh sockets opened (pool miss, age-out, bearer-rotate, or
    /// the silent-reconnect path below).
    pub upgrades: u64,
    /// Cached sockets that died mid-turn and triggered the silent
    /// reopen-and-replay-without-chain-id recovery.
    pub silent_reconnects: u64,
}

impl WsPool {
    pub fn new() -> Self {
        let max = std::env::var(POOL_MAX_ENV)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_POOL_MAX);
        Self {
            conns: LruCache::new(NonZeroUsize::new(max).unwrap_or(NonZeroUsize::MIN)),
            stats: WsPoolStats::default(),
        }
    }
    /// Snapshot the running counters. Cheap (`Copy`); intended for
    /// tracing emission and tests.
    pub fn stats(&self) -> WsPoolStats {
        self.stats
    }

    /// Look up an existing connection for `key`, validating its
    /// bearer/age against the current request. Returns:
    ///
    /// - `Some(conn)` — caller owns it for the turn, must call
    ///   [`Self::release`] on success or drop on failure.
    /// - `None` — pool miss. Caller should `connect()` a fresh `WsConn` and
    ///   insert it via [`Self::release`] after the turn.
    ///
    /// Drops the entry if its bearer has rotated (OAuth refresh) or
    /// the connection is approaching the server-side age limit.
    pub fn checkout(&mut self, key: &PoolKey, current_bearer: &str) -> Option<WsConn> {
        let conn = self.conns.pop(key)?;
        // Bearer rotation: refreshed access token means upstream
        // would reject the existing socket on the next message
        // anyway. Drop and let caller reopen with the new token.
        if conn.bearer != current_bearer {
            return None;
        }
        // Age-out: a 59-minute-old socket would die mid-stream.
        // Reopen here instead, before sending anything.
        if MAX_CONNECTION_AGE <= conn.opened_at.elapsed() {
            return None;
        }
        Some(conn)
    }

    /// Put a connection (newly opened or just-used) back into the
    /// pool. Inserts at the LRU front. Evicts the LRU tail when the
    /// pool was already at capacity.
    pub fn release(&mut self, key: PoolKey, conn: WsConn) {
        self.conns.put(key, conn);
    }

    /// Number of cached connections currently retained by the pool.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.conns.len()
    }

    /// Whether the pool currently has no cached connections.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }
}

impl Default for WsPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe WS pool wrapper used by prompt workers.
///
/// The inner mutex protects only pool bookkeeping. A per-key busy set reserves
/// a conversation chain while its network turn is in flight, so concurrent
/// same-key callers wait for that turn to release/drop the socket instead of
/// opening a second socket for the same chain. Different keys can still run
/// their network turns concurrently.
pub struct SharedWsPool {
    inner: Arc<Mutex<SharedWsPoolInner>>,
    changed: Arc<Condvar>,
}

struct SharedWsPoolInner {
    pool: WsPool,
    busy: HashSet<PoolKey>,
    invalidated_busy: HashSet<PoolKey>,
    abort_wake_generation: u64,
}

impl SharedWsPool {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedWsPoolInner {
                pool: WsPool::new(),
                busy: HashSet::new(),
                invalidated_busy: HashSet::new(),
                abort_wake_generation: 0,
            })),
            changed: Arc::new(Condvar::new()),
        }
    }

    pub fn stats(&self) -> Option<WsPoolStats> {
        self.inner.lock().ok().map(|inner| inner.pool.stats())
    }

    /// Drops the cached socket for one conversation after a transcript
    /// boundary.
    pub fn invalidate(
        &self,
        config: &ResponsesConfig,
        request: &PromptPayload<'_>,
    ) -> Result<(), WsTurnError> {
        let key = PoolKey::for_request(config, request);
        let mut inner = self.lock_inner()?;
        inner.pool.conns.pop(&key);
        if inner.busy.contains(&key) {
            inner.invalidated_busy.insert(key);
        }
        Ok(())
    }

    /// Try to reserve `key` without waiting for an active same-key turn. This
    /// is used by best-effort prewarm requests that run on the provider
    /// main loop: if a real turn already owns the reservation, prewarm
    /// should skip rather than delaying cancellation, worker output,
    /// PromptDone, or ACK handling.
    fn try_checkout(
        &self,
        key: &PoolKey,
        current_bearer: &str,
    ) -> Result<TryCheckout, WsTurnError> {
        let mut inner = self.lock_inner()?;
        if inner.busy.contains(key) {
            return Ok(TryCheckout::Busy);
        }
        inner.busy.insert(key.clone());
        Ok(TryCheckout::Reserved(
            inner.pool.checkout(key, current_bearer),
        ))
    }

    /// Reserve `key`, aborting promptly if the abort source fires while a
    /// same-key worker owns the reservation. This is used by prompt turns so a
    /// targeted cancel cannot leave a worker blocked in the pool and then later
    /// send a stale network request after the canceled turn releases.
    fn checkout_until(
        &self,
        key: &PoolKey,
        current_bearer: &str,
        abort: &mut impl TurnAbort,
    ) -> Result<Option<WsConn>, WsTurnError> {
        let _abort_waker = self.register_checkout_abort_waker(abort);
        let mut inner = self.lock_inner()?;
        while inner.busy.contains(key) {
            if abort.is_aborted() {
                return Err(WsTurnError::Canceled);
            }
            let wake_generation = inner.abort_wake_generation;
            while inner.busy.contains(key) && inner.abort_wake_generation == wake_generation {
                inner = self.changed.wait(inner).map_err(pool_poisoned)?;
            }
        }
        if abort.is_aborted() {
            return Err(WsTurnError::Canceled);
        }
        inner.busy.insert(key.clone());
        Ok(inner.pool.checkout(key, current_bearer))
    }

    fn register_checkout_abort_waker(
        &self,
        abort: &mut impl TurnAbort,
    ) -> Box<dyn crate::TurnAbortWaker> {
        let inner = Arc::clone(&self.inner);
        let changed = Arc::clone(&self.changed);
        abort.register_waker(Arc::new(move || {
            if let Ok(mut pool_inner) = inner.lock() {
                pool_inner.abort_wake_generation = pool_inner.abort_wake_generation.wrapping_add(1);
            }
            changed.notify_all();
        }))
    }

    fn release(&self, key: PoolKey, conn: WsConn) -> Result<(), WsTurnError> {
        let mut inner = self.lock_inner()?;
        if !inner.invalidated_busy.remove(&key) {
            inner.pool.release(key.clone(), conn);
        }
        inner.busy.remove(&key);
        self.changed.notify_all();
        Ok(())
    }

    fn abandon(&self, key: &PoolKey) -> Result<(), WsTurnError> {
        let mut inner = self.lock_inner()?;
        inner.busy.remove(key);
        inner.invalidated_busy.remove(key);
        self.changed.notify_all();
        Ok(())
    }

    fn bump_silent_reconnects(&self) -> Result<u64, WsTurnError> {
        let mut inner = self.lock_inner()?;
        inner.pool.stats.silent_reconnects += 1;
        Ok(inner.pool.stats.silent_reconnects)
    }

    fn record_fresh_open(&self) -> Result<(), WsTurnError> {
        let mut inner = self.lock_inner()?;
        inner.pool.stats.upgrades += 1;
        Ok(())
    }

    fn lock_inner(&self) -> Result<std::sync::MutexGuard<'_, SharedWsPoolInner>, WsTurnError> {
        self.inner.lock().map_err(pool_poisoned)
    }
}

impl Default for SharedWsPool {
    fn default() -> Self {
        Self::new()
    }
}

fn pool_poisoned<T>(error: std::sync::PoisonError<T>) -> WsTurnError {
    WsTurnError::Other(LlmError::HttpStatus(
        0,
        format!("WS pool poisoned: {error}"),
    ))
}

enum TryCheckout {
    Reserved(Option<WsConn>),
    Busy,
}

enum VcrTurnSetup {
    Replayed(Box<crate::common::StreamState>),
    Live {
        record_config: Option<tau_vcr::VcrConfig>,
    },
}

enum CachedSharedTurn {
    Completed(Box<crate::common::StreamState>),
    // The cached socket was already removed from the pool and the `PoolKey`
    // reservation is still held. The caller must immediately run the fresh
    // retry for the same key so no competing same-key worker can open a second
    // chain socket between the recoverable failure and replacement release.
    RetryFresh,
}

struct SharedTurnContext<'a, 'request> {
    // Owns the reserved key for the duration of one cached or fresh attempt.
    // After `RetryFresh`, constructing the fresh context with the same key keeps
    // the original reservation alive until `run_fresh` releases or abandons it.
    pool: &'a SharedWsPool,
    key: PoolKey,
    config: &'a ResponsesConfig,
    agent_prompt_id: &'a str,
    request: &'a crate::common::PromptPayload<'request>,
    record_config: Option<&'a tau_vcr::VcrConfig>,
}

/// WS dispatch failed in a way the caller can classify.
#[derive(Debug)]
pub enum WsTurnError {
    Canceled,
    Other(LlmError),
}

impl WsTurnError {
    pub fn into_llm_error(self) -> LlmError {
        match self {
            Self::Canceled => LlmError::Canceled,
            Self::Other(error) => error,
        }
    }
}

fn store_ws_vcr_recording(
    vcr_config: Option<&tau_vcr::VcrConfig>,
    request: &crate::common::PromptPayload<'_>,
    agent_prompt_id: &str,
    request_body: Option<serde_json::Value>,
    stream: Option<super::ProviderRawEventStream>,
) -> Result<(), LlmError> {
    let Some(vcr_config) = vcr_config else {
        return Ok(());
    };
    let (Some(request_body), Some(stream)) = (request_body, stream) else {
        return Ok(());
    };
    let key = super::provider_vcr_key(
        request,
        agent_prompt_id,
        tau_proto::ProviderBackendTransport::Websocket,
    );
    let cassette = super::ProviderStreamCassette {
        version: super::PROVIDER_STREAM_CASSETTE_VERSION,
        request: request_body,
        stream,
    };
    vcr_config
        .store()
        .put(&key, &cassette)
        .map_err(LlmError::Vcr)
}

fn prepare_vcr_turn(
    config: &ResponsesConfig,
    agent_prompt_id: &str,
    request: &crate::common::PromptPayload<'_>,
    on_update: &mut impl FnMut(&crate::common::StreamState),
) -> Result<VcrTurnSetup, WsTurnError> {
    let vcr_config = super::load_vcr_config().map_err(WsTurnError::Other)?;
    let Some(vcr_config) = vcr_config else {
        return Ok(VcrTurnSetup::Live {
            record_config: None,
        });
    };

    if let Some(state) =
        super::ws::run_vcr_replay_turn(&vcr_config, config, agent_prompt_id, request, on_update)
            .map_err(WsTurnError::Other)?
    {
        return Ok(VcrTurnSetup::Replayed(Box::new(state)));
    }

    Ok(VcrTurnSetup::Live {
        record_config: (vcr_config.mode == tau_vcr::VcrMode::RecordIfMissing).then_some(vcr_config),
    })
}

fn recording_stream(
    record_config: Option<&tau_vcr::VcrConfig>,
) -> Option<super::ProviderRawEventStream> {
    record_config.map(|_| super::ProviderRawEventStream::default())
}

/// Test-only convenience wrapper that wires `checkout` → `WsConn::run_turn` →
/// `release` together with reopen-on-miss semantics without the production
/// mutex wrapper.
///
/// Transparent reconnect: the Codex WS endpoint's
/// `previous_response_id` cache is **connection-local** (per the
/// OpenAI deployment-checklist WS guide). A fresh socket from
/// `WsConn::connect` has an empty chain cache, so the request builder
/// replays the full prompt once over the new WS and releases that
/// socket back into the pool so the following turn is warm again.
#[cfg(test)]
pub fn run_turn_through_pool(
    pool: &mut WsPool,
    config: &ResponsesConfig,
    session_id: &str,
    agent_prompt_id: &str,
    request: &crate::common::PromptPayload<'_>,
    on_update: &mut impl FnMut(&crate::common::StreamState),
) -> Result<crate::common::StreamState, WsTurnError> {
    let vcr_config = super::load_vcr_config().map_err(WsTurnError::Other)?;
    let vcr_record_config = if let Some(vcr_config) = vcr_config.as_ref() {
        if let Some(state) =
            super::ws::run_vcr_replay_turn(vcr_config, config, agent_prompt_id, request, on_update)
                .map_err(WsTurnError::Other)?
        {
            return Ok(state);
        }
        (vcr_config.mode == tau_vcr::VcrMode::RecordIfMissing).then(|| vcr_config.clone())
    } else {
        None
    };

    let key = PoolKey::for_request(config, request);

    // First attempt: prefer a warm cached connection so the
    // connection-local chain cache stays useful.
    if let Some(mut conn) = pool.checkout(&key, &config.api_key) {
        let mut recording_stream = vcr_record_config
            .as_ref()
            .map(|_| super::ProviderRawEventStream::default());
        let mut abort = NeverAbort;
        match conn.run_turn(
            config,
            agent_prompt_id,
            request,
            recording_stream.as_mut(),
            &mut abort,
            on_update,
        ) {
            Ok(turn) => {
                let state = turn.state;
                let request_body = turn.request_body;
                pool.release(key, conn);
                store_ws_vcr_recording(
                    vcr_record_config.as_ref(),
                    request,
                    agent_prompt_id,
                    request_body,
                    recording_stream,
                )
                .map_err(WsTurnError::Other)?;
                return Ok(state);
            }
            // Recording intentionally does not silently reconnect: a retry can
            // change the WS request shape (warm `previous_response_id` vs.
            // fresh full replay), which makes cassette matching ambiguous.
            Err(other) if vcr_record_config.is_some() => {
                drop(conn);
                return Err(WsTurnError::Other(other));
            }
            Err(err) if is_recoverable_ws_error(&err) => {
                pool.stats.silent_reconnects += 1;
                tracing::info!(
                    target: crate::LOG_TARGET,
                    session_id,
                    error = %err,
                    silent_reconnects = pool.stats.silent_reconnects,
                    "Codex WS connection lost mid-turn",
                );
                drop(conn);
                // Fall through to the fresh-open path below. If this was a
                // chained turn, the fresh request will rebuild WS warmth with
                // one full replay.
            }
            Err(other) => {
                drop(conn);
                return Err(WsTurnError::Other(other));
            }
        }
    }

    // Fresh socket path. The chain cache here is empty by definition, so pay
    // one cold full replay on WS. That is cheaper over the next turns than
    // switching to HTTP and staying cold.
    let mut conn = WsConn::connect(config, &key.thread_id).map_err(WsTurnError::Other)?;
    pool.stats.upgrades += 1;
    let mut recording_stream = vcr_record_config
        .as_ref()
        .map(|_| super::ProviderRawEventStream::default());
    let mut abort = NeverAbort;
    match conn.run_turn(
        config,
        agent_prompt_id,
        request,
        recording_stream.as_mut(),
        &mut abort,
        on_update,
    ) {
        Ok(turn) => {
            let state = turn.state;
            let request_body = turn.request_body;
            pool.release(key, conn);
            store_ws_vcr_recording(
                vcr_record_config.as_ref(),
                request,
                agent_prompt_id,
                request_body,
                recording_stream,
            )
            .map_err(WsTurnError::Other)?;
            Ok(state)
        }
        Err(err) => {
            drop(conn);
            Err(WsTurnError::Other(err))
        }
    }
}

/// Thread-safe prompt-worker entry point. Shared-pool bookkeeping is locked
/// only while checking out/reserving a key, updating stats, or releasing a
/// successful connection. The network turn runs without the lock, so unrelated
/// prompt workers can use their own pooled sockets concurrently; same-key
/// callers wait on the reservation to preserve one chain per socket.
pub fn run_turn_through_shared_pool(
    pool: &SharedWsPool,
    config: &ResponsesConfig,
    agent_prompt_id: &str,
    request: &crate::common::PromptPayload<'_>,
    abort: &mut impl TurnAbort,
    on_update: &mut impl FnMut(&crate::common::StreamState),
) -> Result<crate::common::StreamState, WsTurnError> {
    let record_config = match prepare_vcr_turn(config, agent_prompt_id, request, on_update)? {
        VcrTurnSetup::Replayed(state) => return Ok(*state),
        VcrTurnSetup::Live { record_config } => record_config,
    };

    let session_id = request.session_id.as_str();
    let key = PoolKey::for_request(config, request);

    if let Some(conn) = pool.checkout_until(&key, &config.api_key, abort)? {
        let turn_context = SharedTurnContext {
            pool,
            key: key.clone(),
            config,
            agent_prompt_id,
            request,
            record_config: record_config.as_ref(),
        };
        match turn_context.run_cached(conn, session_id, abort, on_update)? {
            CachedSharedTurn::Completed(state) => return Ok(*state),
            CachedSharedTurn::RetryFresh => {}
        }
    }

    SharedTurnContext {
        pool,
        key,
        config,
        agent_prompt_id,
        request,
        record_config: record_config.as_ref(),
    }
    .run_fresh(abort, on_update)
}

impl<'a, 'request> SharedTurnContext<'a, 'request> {
    fn run_cached(
        self,
        mut conn: WsConn,
        session_id: &str,
        abort: &mut impl TurnAbort,
        on_update: &mut impl FnMut(&crate::common::StreamState),
    ) -> Result<CachedSharedTurn, WsTurnError> {
        if abort.is_aborted() {
            self.pool.release(self.key, conn)?;
            return Err(WsTurnError::Canceled);
        }

        let mut stream = recording_stream(self.record_config);
        match conn.run_turn(
            self.config,
            self.agent_prompt_id,
            self.request,
            stream.as_mut(),
            abort,
            on_update,
        ) {
            Ok(turn) => self
                .release_and_store_recording(conn, turn, stream)
                .map(Box::new)
                .map(CachedSharedTurn::Completed),
            // Recording intentionally does not silently reconnect: a retry can
            // change the WS request shape (warm `previous_response_id` vs.
            // fresh full replay), which makes cassette matching ambiguous.
            Err(other) if self.record_config.is_some() => {
                drop(conn);
                self.pool.abandon(&self.key)?;
                Err(WsTurnError::Other(other))
            }
            Err(err) if is_recoverable_ws_error(&err) => {
                let silent_reconnects = self.pool.bump_silent_reconnects()?;
                tracing::info!(
                    target: crate::LOG_TARGET,
                    session_id,
                    error = %err,
                    silent_reconnects,
                    "Codex WS connection lost mid-turn",
                );
                drop(conn);
                Ok(CachedSharedTurn::RetryFresh)
            }
            Err(other) => {
                drop(conn);
                self.pool.abandon(&self.key)?;
                Err(WsTurnError::Other(other))
            }
        }
    }

    fn run_fresh(
        self,
        abort: &mut impl TurnAbort,
        on_update: &mut impl FnMut(&crate::common::StreamState),
    ) -> Result<crate::common::StreamState, WsTurnError> {
        if abort.is_aborted() {
            self.pool.abandon(&self.key)?;
            return Err(WsTurnError::Canceled);
        }
        let mut conn = match WsConn::connect(self.config, &self.key.thread_id) {
            Ok(conn) => conn,
            Err(error) => {
                self.pool.abandon(&self.key)?;
                return Err(WsTurnError::Other(error));
            }
        };
        if abort.is_aborted() {
            drop(conn);
            self.pool.abandon(&self.key)?;
            return Err(WsTurnError::Canceled);
        }
        self.pool.record_fresh_open()?;
        let mut stream = recording_stream(self.record_config);
        match conn.run_turn(
            self.config,
            self.agent_prompt_id,
            self.request,
            stream.as_mut(),
            abort,
            on_update,
        ) {
            Ok(turn) => self.release_and_store_recording(conn, turn, stream),
            Err(err) => {
                drop(conn);
                self.pool.abandon(&self.key)?;
                Err(WsTurnError::Other(err))
            }
        }
    }

    fn release_and_store_recording(
        self,
        conn: WsConn,
        turn: super::ws::WsTurnResult,
        recording_stream: Option<super::ProviderRawEventStream>,
    ) -> Result<crate::common::StreamState, WsTurnError> {
        let state = turn.state;
        let request_body = turn.request_body;
        self.pool.release(self.key, conn)?;
        store_ws_vcr_recording(
            self.record_config,
            self.request,
            self.agent_prompt_id,
            request_body,
            recording_stream,
        )
        .map_err(WsTurnError::Other)?;
        Ok(state)
    }
}

/// Send a best-effort non-generating prewarm over the same pooled WS
/// connection a later real turn for this prompt-cache thread will use. Unlike
/// real turns, a failed cached socket is simply dropped and retried
/// once on a fresh socket; no stateful chain id exists on prewarm.
#[cfg(test)]
pub fn run_prewarm_through_pool(
    pool: &mut WsPool,
    config: &ResponsesConfig,
    session_id: &str,
    request: &crate::common::PromptPayload<'_>,
) -> Result<crate::common::StreamState, LlmError> {
    let key = PoolKey::for_request(config, request);

    if let Some(mut conn) = pool.checkout(&key, &config.api_key) {
        match conn.run_prewarm(config, request) {
            Ok(state) => {
                pool.release(key, conn);
                return Ok(state);
            }
            Err(err) if is_recoverable_ws_error(&err) => {
                pool.stats.silent_reconnects += 1;
                tracing::info!(
                    target: crate::LOG_TARGET,
                    session_id,
                    error = %err,
                    "Codex WS connection lost during prewarm; reopening",
                );
                drop(conn);
            }
            Err(other) => {
                drop(conn);
                return Err(other);
            }
        }
    }

    let mut conn = WsConn::connect(config, &key.thread_id)?;
    pool.stats.upgrades += 1;
    match conn.run_prewarm(config, request) {
        Ok(state) => {
            pool.release(key, conn);
            Ok(state)
        }
        Err(err) => {
            drop(conn);
            Err(err)
        }
    }
}

/// Thread-safe prewarm entry point. It reserves only the matching key while the
/// network prewarm is in flight, so prompt workers on other keys can continue
/// to check out/release pooled sockets concurrently. If that key is already
/// reserved, prewarm is skipped because it is best-effort main-loop work.
pub fn run_prewarm_through_shared_pool(
    pool: &SharedWsPool,
    config: &ResponsesConfig,
    session_id: &str,
    request: &crate::common::PromptPayload<'_>,
) -> Result<crate::common::StreamState, LlmError> {
    let key = PoolKey::for_request(config, request);

    if let TryCheckout::Reserved(cached) = pool
        .try_checkout(&key, &config.api_key)
        .map_err(WsTurnError::into_llm_error)?
    {
        if let Some(mut conn) = cached {
            match conn.run_prewarm(config, request) {
                Ok(state) => {
                    pool.release(key, conn)
                        .map_err(WsTurnError::into_llm_error)?;
                    return Ok(state);
                }
                Err(err) if is_recoverable_ws_error(&err) => {
                    pool.bump_silent_reconnects()
                        .map_err(WsTurnError::into_llm_error)?;
                    tracing::info!(
                        target: crate::LOG_TARGET,
                        session_id,
                        error = %err,
                        "Codex WS connection lost during prewarm; reopening",
                    );
                    drop(conn);
                }
                Err(other) => {
                    drop(conn);
                    pool.abandon(&key).map_err(WsTurnError::into_llm_error)?;
                    return Err(other);
                }
            }
        }
    } else {
        tracing::debug!(
            target: crate::LOG_TARGET,
            session_id,
            "skipping prompt prewarm: websocket pool key is busy",
        );
        return Ok(crate::common::StreamState::new());
    }

    let mut conn = match WsConn::connect(config, &key.thread_id) {
        Ok(conn) => conn,
        Err(error) => {
            pool.abandon(&key).map_err(WsTurnError::into_llm_error)?;
            return Err(error);
        }
    };
    pool.record_fresh_open()
        .map_err(WsTurnError::into_llm_error)?;
    match conn.run_prewarm(config, request) {
        Ok(state) => {
            pool.release(key, conn)
                .map_err(WsTurnError::into_llm_error)?;
            Ok(state)
        }
        Err(err) => {
            drop(conn);
            pool.abandon(&key).map_err(WsTurnError::into_llm_error)?;
            Err(err)
        }
    }
}

/// Errors from `WsConn::run_turn` that mean "this socket is dead,
/// but the *next* socket can probably serve the turn." Caller's job
/// is to reopen and retry once silently rather than letting the outer
/// retry loop burn a backoff on the same broken state.
///
/// Every `run_turn` error path lands here as `LlmError::HttpStatus(0,
/// "stream error: ...")` — and by construction every one of them
/// indicates "this socket can't serve another turn":
///
/// - Transport-level closes: tungstenite raised `ConnectionClosed`,
///   `AlreadyClosed`, or an IO break; the server sent a close frame mid-stream;
///   keepalive ping or turn-send failed write-side.
/// - Task-supervision failures: the per-conn reader or writer task exited or
///   got aborted — the socket they owned is gone.
/// - Server-level stale-chain: an `error` event whose message says the
///   `previous_response_id` we just sent doesn't exist on this socket. Same
///   root cause as a transport close — the previous socket carrying that chain
///   id is gone — just surfaced through the JSON event stream instead of a TCP
///   close.
///
/// The matcher is therefore deliberately broad: anything with the
/// `"stream error:"` prefix from `run_turn` is recoverable. The
/// alternative — a narrow allow-list — silently leaks any new
/// failure mode (e.g. the previous `"ws writer task gone"`) to the
/// outer retry loop, which burns a backoff sleep on the same dead
/// socket. Other `LlmError` variants and non-stream-prefixed bodies
/// fall through unchanged.
///
/// Carve-outs: account-level caps (usage_limit_reached, rate limit,
/// quota) reach us with the same prefix because they ride the same
/// `error` event, but the connection is fine — reopening just burns a
/// fresh upgrade against an upstream that's about to reject every
/// request the same way. Local provider-stream idle watchdog failures
/// also use the same prefix, but replaying them would keep a stuck turn
/// in-flight for another idle window. Defer those to the outer
/// classifier (`LlmError::retry_after`), which returns `None` and
/// surfaces the error immediately.
fn is_recoverable_ws_error(err: &LlmError) -> bool {
    let LlmError::HttpStatus(0, body) = err else {
        return false;
    };
    if !body.starts_with("stream error:") {
        return false;
    }
    !crate::common::is_account_limit_body(body)
        && !crate::common::is_provider_stream_idle_timeout_body(body)
}

#[cfg(test)]
mod tests;
