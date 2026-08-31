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
use std::{cell as path_std_cell, collections as path_std_collections, time as path_std_time};

use lru::LruCache;
use tau_provider::private_attempt_trace as private_trace;

use super::ResponsesConfig;
use super::ws::{ResponseMode, WsConn};
#[cfg(test)]
use crate::NeverAbort;
use crate::common::{LlmError, PromptPayload};
use crate::{TurnAbort, attempt_failure as path_crate_attempt_failure};

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
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PoolKey {
    /// Filename-derived provider namespace owning this socket.
    pub profile_namespace: tau_proto::ProviderName,
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

impl std::fmt::Debug for PoolKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PoolKey")
            .field("profile_namespace", &self.profile_namespace.as_str())
            .field("base_url", &self.base_url)
            .field("account_id", &self.account_id)
            .field("thread_id", &self.thread_id)
            .finish()
    }
}

impl PoolKey {
    /// Build the socket key for a prompt request.
    ///
    /// The request's prompt-cache UUID becomes the upstream `thread-id` and
    /// `session-id` headers, so it is part of the pool key rather than a
    /// per-turn detail.
    pub fn for_request(config: &ResponsesConfig, request: &PromptPayload<'_>) -> Self {
        Self {
            profile_namespace: config.profile_namespace.clone(),
            base_url: config.base_url.clone(),
            account_id: config.account_id.clone(),
            thread_id: request.prompt_cache_key(&config.base_url, config.mode),
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
            .filter(|&n| 0 < n)
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
#[cfg(test)]
type CheckoutWaitHook = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Clone)]
pub struct SharedWsPool {
    inner: Arc<Mutex<SharedWsPoolInner>>,
    changed: Arc<Condvar>,
    /// Immutable outbound policy used by every fresh connection.
    network: Arc<tau_provider::OutboundNetworkPolicy>,
    /// Test-only observation hook fired at the exact same-key wait boundary.
    #[cfg(test)]
    checkout_wait_hook: Arc<Mutex<Option<CheckoutWaitHook>>>,
}

struct SharedWsPoolInner {
    pool: WsPool,
    busy: HashSet<PoolKey>,
    invalidated_busy: HashSet<PoolKey>,
    /// Exact generation allowed to cancel or retire each active prewarm key.
    prewarm_owners: std::collections::HashMap<PoolKey, u64>,
    /// Wrapping source of process-local prewarm ownership generations.
    next_prewarm_owner: u64,
    abort_wake_generation: u64,
}

impl SharedWsPool {
    /// Create a pool whose fresh sockets share one startup network policy.
    pub(crate) fn new(network: Arc<tau_provider::OutboundNetworkPolicy>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedWsPoolInner {
                pool: WsPool::new(),
                busy: HashSet::new(),
                invalidated_busy: HashSet::new(),
                prewarm_owners: path_std_collections::HashMap::new(),
                next_prewarm_owner: 0,
                abort_wake_generation: 0,
            })),
            changed: Arc::new(Condvar::new()),
            network,
            #[cfg(test)]
            checkout_wait_hook: Arc::new(Mutex::new(None)),
        }
    }

    /// Installs a test observer for the exact busy-key condition-variable wait.
    #[cfg(test)]
    fn set_checkout_wait_hook(&self, hook: CheckoutWaitHook) {
        *self
            .checkout_wait_hook
            .lock()
            .expect("checkout wait hook lock") = Some(hook);
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

    /// Assigns an exact generation to the current prewarm reservation.
    fn claim_prewarm(&self, key: &PoolKey) -> Result<u64, WsTurnError> {
        let mut inner = self.lock_inner()?;
        let generation = inner.next_prewarm_owner;
        inner.next_prewarm_owner = inner.next_prewarm_owner.wrapping_add(1);
        inner.prewarm_owners.insert(key.clone(), generation);
        Ok(generation)
    }

    /// Invalidates only if the callback still belongs to the same prewarm.
    fn invalidate_prewarm(&self, key: &PoolKey, generation: u64) -> Result<(), WsTurnError> {
        let mut inner = self.lock_inner()?;
        if inner.prewarm_owners.get(key) != Some(&generation) {
            return Ok(());
        }
        inner.pool.conns.pop(key);
        if inner.busy.contains(key) {
            inner.invalidated_busy.insert(key.clone());
        }
        Ok(())
    }

    /// Abandons only the exact prewarm generation that still owns the busy key.
    fn abandon_prewarm(&self, key: &PoolKey, generation: u64) -> Result<(), WsTurnError> {
        let mut inner = self.lock_inner()?;
        if inner.prewarm_owners.get(key) != Some(&generation) {
            return Ok(());
        }
        inner.pool.conns.pop(key);
        inner.prewarm_owners.remove(key);
        inner.busy.remove(key);
        inner.invalidated_busy.remove(key);
        self.changed.notify_all();
        Ok(())
    }

    /// Invalidates every cached and currently reserved socket.
    ///
    /// Reserved sockets remain owned by their worker until it finishes, but
    /// their eventual release is discarded. This lets the
    /// provider invalidate prewarm work atomically across profile or session
    /// changes without taking socket ownership away from an active thread.
    pub(crate) fn invalidate_all(&self) -> Result<(), WsTurnError> {
        let mut inner = self.lock_inner()?;
        inner.pool.conns.clear();
        let busy = inner.busy.iter().cloned().collect::<Vec<_>>();
        inner.invalidated_busy.extend(busy);
        Ok(())
    }

    /// Invalidates cached and reserved sockets for one provider namespace.
    pub(crate) fn invalidate_profile(
        &self,
        provider: &tau_proto::ProviderName,
    ) -> Result<(), WsTurnError> {
        let mut inner = self.lock_inner()?;
        let cached = inner
            .pool
            .conns
            .iter()
            .filter(|(key, _)| &key.profile_namespace == provider)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in cached {
            inner.pool.conns.pop(&key);
        }
        let busy = inner
            .busy
            .iter()
            .filter(|key| &key.profile_namespace == provider)
            .cloned()
            .collect::<Vec<_>>();
        inner.invalidated_busy.extend(busy);
        Ok(())
    }

    /// Try to reserve `key` without waiting for an active same-key turn. This
    /// is used by best-effort supervised prewarm workers: if a real turn
    /// already owns the reservation, prewarm should skip rather than
    /// creating duplicate same-key work.
    fn try_checkout(
        &self,
        key: &PoolKey,
        current_bearer: &str,
    ) -> Result<TryCheckout, WsTurnError> {
        let mut inner = self.lock_inner()?;
        if inner.busy.contains(key) {
            return Ok(TryCheckout::Busy);
        }
        inner.prewarm_owners.remove(key);
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
                #[cfg(test)]
                if let Some(hook) = self
                    .checkout_wait_hook
                    .lock()
                    .expect("checkout wait hook lock")
                    .clone()
                {
                    hook();
                }
                inner = self.changed.wait(inner).map_err(pool_poisoned)?;
            }
        }
        if abort.is_aborted() {
            return Err(WsTurnError::Canceled);
        }
        inner.prewarm_owners.remove(key);
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

    /// Installs a prewarmed socket while retaining its reservation until the
    /// cancellation callback has been unregistered.
    fn stage_prewarm_release(&self, key: &PoolKey, conn: WsConn) -> Result<(), WsTurnError> {
        let mut inner = self.lock_inner()?;
        if !inner.invalidated_busy.contains(key) {
            inner.pool.release(key.clone(), conn);
        }
        Ok(())
    }

    /// Completes a staged prewarm release after cancellation can no longer
    /// target this owner, then wakes a same-key prompt waiter.
    fn finish_prewarm_release(&self, key: &PoolKey, generation: u64) -> Result<(), WsTurnError> {
        let mut inner = self.lock_inner()?;
        if inner.prewarm_owners.get(key) != Some(&generation) {
            return Ok(());
        }
        if inner.invalidated_busy.remove(key) {
            inner.pool.conns.pop(key);
        }
        inner.prewarm_owners.remove(key);
        inner.busy.remove(key);
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

    fn connect_reserved_fresh<A, C>(
        &self,
        key: &PoolKey,
        config: &ResponsesConfig,
        abort: &mut A,
        connect: C,
    ) -> Result<WsConn, WsTurnError>
    where
        A: TurnAbort,
        C: FnOnce(&ResponsesConfig, &str, &mut A) -> Result<WsConn, LlmError>,
    {
        if abort.is_aborted() {
            self.abandon(key)?;
            return Err(WsTurnError::Canceled);
        }
        let conn = match connect(config, &key.thread_id, abort) {
            Ok(conn) => conn,
            Err(LlmError::Canceled) => {
                self.abandon(key)?;
                return Err(WsTurnError::Canceled);
            }
            Err(error) => {
                self.abandon(key)?;
                return Err(WsTurnError::Other(error));
            }
        };
        if abort.is_aborted() {
            drop(conn);
            self.abandon(key)?;
            return Err(WsTurnError::Canceled);
        }
        Ok(conn)
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

/// Scope-bound ownership of one prewarm pool reservation.
struct PrewarmReservation<'a> {
    /// Pool whose busy key must be released.
    pool: &'a SharedWsPool,
    /// Exact reserved key.
    key: PoolKey,
    /// Generation proving this guard still owns the key.
    generation: u64,
    /// False only after normal staged release completed.
    armed: bool,
}

impl PrewarmReservation<'_> {
    /// Prevents drop cleanup after normal pool release.
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Publishes one staged socket only after cancellation callback retirement.
    fn publish(
        &mut self,
        conn: WsConn,
        cancel_guard: Box<dyn crate::TurnAbortWaker>,
        abort: &mut impl TurnAbort,
    ) -> Result<(), WsTurnError> {
        self.pool.stage_prewarm_release(&self.key, conn)?;
        // Busy ownership remains held while this joins an already-started
        // cancellation callback or unregisters before later cancellation.
        drop(cancel_guard);
        if abort.is_aborted() {
            return Err(WsTurnError::Canceled);
        }
        self.pool
            .finish_prewarm_release(&self.key, self.generation)?;
        self.disarm();
        Ok(())
    }
}

impl Drop for PrewarmReservation<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.pool.abandon_prewarm(&self.key, self.generation);
        }
    }
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
    RetryFresh {
        /// Cumulative transport bytes retained across the discarded attempt.
        response_bytes: u64,
        /// Whether the provider explicitly rejected the connection-local chain.
        stale_chain: bool,
    },
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
    /// Response event language selected by the caller.
    response_mode: ResponseMode,
}

/// Private repair evidence accumulated from response-state callbacks.
#[derive(Default)]
struct TurnObservation {
    /// Highest cumulative response-byte count observed in this dispatch.
    response_bytes: u64,
    /// Whether any model-semantic output made transparent replay unsafe.
    semantic_progress: bool,
}

/// Retain private attempt evidence for every transport source, then publish
/// response state only for ordinary inference.
fn observe_attempt_state(
    correlation: &mut Option<&mut crate::attempt_failure::AttemptCaptureCorrelation>,
    response_mode: ResponseMode,
    on_update: &mut impl FnMut(crate::StreamUpdate<'_>),
    state: &crate::common::StreamState,
) {
    if let Some(correlation) = correlation.as_deref_mut() {
        correlation.observe_stream(state);
    }
    if response_mode == ResponseMode::Ordinary {
        on_update(crate::StreamUpdate::Response(state));
    }
}

impl TurnObservation {
    /// Starts a repair dispatch with bytes spent by its discarded predecessor.
    fn with_carried_response_bytes(response_bytes: u64) -> Self {
        Self {
            response_bytes,
            semantic_progress: false,
        }
    }

    /// Incorporates one parser state without exposing it outside the pool.
    fn observe(&mut self, state: &crate::common::StreamState) {
        self.response_bytes = self.response_bytes.max(state.response_bytes_received());
        self.semantic_progress |= state.has_semantic_progress();
    }
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
    response_mode: ResponseMode,
    on_update: &mut impl FnMut(&crate::common::StreamState),
) -> Result<VcrTurnSetup, WsTurnError> {
    let vcr_config = super::load_vcr_config().map_err(WsTurnError::Other)?;
    let Some(vcr_config) = vcr_config else {
        return Ok(VcrTurnSetup::Live {
            record_config: None,
        });
    };

    if let Some(state) = super::ws::run_vcr_replay_turn(
        &vcr_config,
        config,
        agent_prompt_id,
        request,
        response_mode,
        on_update,
    )
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
        if let Some(state) = super::ws::run_vcr_replay_turn(
            vcr_config,
            config,
            agent_prompt_id,
            request,
            ResponseMode::Ordinary,
            on_update,
        )
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
            None,
            recording_stream.as_mut(),
            &mut abort,
            &mut |_| {},
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
    let mut abort = NeverAbort;
    let mut conn = WsConn::connect(
        config,
        &key.thread_id,
        &crate::test_network_policy(),
        &mut abort,
    )
    .map_err(WsTurnError::Other)?;
    pool.stats.upgrades += 1;
    let mut recording_stream = vcr_record_config
        .as_ref()
        .map(|_| super::ProviderRawEventStream::default());
    match conn.run_turn(
        config,
        agent_prompt_id,
        request,
        None,
        recording_stream.as_mut(),
        &mut abort,
        &mut |_| {},
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
    correlation: Option<&mut crate::attempt_failure::AttemptCaptureCorrelation>,
    abort: &mut impl TurnAbort,
    on_update: &mut impl FnMut(crate::StreamUpdate<'_>),
) -> Result<crate::common::StreamState, WsTurnError> {
    run_turn_through_shared_pool_observed(
        pool,
        config,
        agent_prompt_id,
        request,
        correlation,
        abort,
        on_update,
        &mut None,
    )
}

/// Internal observed entry point preserving the plain supported API.
#[expect(
    clippy::too_many_arguments,
    reason = "private observation is an inert carrier"
)]
pub(crate) fn run_turn_through_shared_pool_observed(
    pool: &SharedWsPool,
    config: &ResponsesConfig,
    agent_prompt_id: &str,
    request: &crate::common::PromptPayload<'_>,
    correlation: Option<&mut crate::attempt_failure::AttemptCaptureCorrelation>,
    abort: &mut impl TurnAbort,
    on_update: &mut impl FnMut(crate::StreamUpdate<'_>),
    private_trace: &mut Option<private_trace::AttemptTrace>,
) -> Result<crate::common::StreamState, WsTurnError> {
    run_response_through_shared_pool(
        pool,
        config,
        agent_prompt_id,
        request,
        correlation,
        ResponseMode::Ordinary,
        abort,
        on_update,
        private_trace,
    )
}

/// Thread-safe native-compaction entry point using original-event validation.
pub fn run_compact_through_shared_pool(
    pool: &SharedWsPool,
    config: &ResponsesConfig,
    agent_prompt_id: &str,
    request: &crate::common::PromptPayload<'_>,
    correlation: Option<&mut crate::attempt_failure::AttemptCaptureCorrelation>,
    abort: &mut impl TurnAbort,
    on_update: &mut impl FnMut(crate::StreamUpdate<'_>),
) -> Result<crate::common::StreamState, WsTurnError> {
    run_compact_through_shared_pool_observed(
        pool,
        config,
        agent_prompt_id,
        request,
        correlation,
        abort,
        on_update,
        &mut None,
    )
}

/// Internal observed compact entry point preserving the plain supported API.
#[expect(
    clippy::too_many_arguments,
    reason = "private observation is an inert carrier"
)]
pub(crate) fn run_compact_through_shared_pool_observed(
    pool: &SharedWsPool,
    config: &ResponsesConfig,
    agent_prompt_id: &str,
    request: &crate::common::PromptPayload<'_>,
    correlation: Option<&mut crate::attempt_failure::AttemptCaptureCorrelation>,
    abort: &mut impl TurnAbort,
    on_update: &mut impl FnMut(crate::StreamUpdate<'_>),
    private_trace: &mut Option<private_trace::AttemptTrace>,
) -> Result<crate::common::StreamState, WsTurnError> {
    run_response_through_shared_pool(
        pool,
        config,
        agent_prompt_id,
        request,
        correlation,
        ResponseMode::Compact,
        abort,
        on_update,
        private_trace,
    )
}

/// Runs one pooled response with an explicitly selected event language.
#[expect(
    clippy::too_many_arguments,
    reason = "transport lifecycle callbacks remain separate typed boundaries"
)]
fn run_response_through_shared_pool(
    pool: &SharedWsPool,
    config: &ResponsesConfig,
    agent_prompt_id: &str,
    request: &crate::common::PromptPayload<'_>,
    correlation: Option<&mut crate::attempt_failure::AttemptCaptureCorrelation>,
    response_mode: ResponseMode,
    abort: &mut impl TurnAbort,
    on_update: &mut impl FnMut(crate::StreamUpdate<'_>),
    private_trace: &mut Option<private_trace::AttemptTrace>,
) -> Result<crate::common::StreamState, WsTurnError> {
    let mut correlation = correlation;
    let record_config = match prepare_vcr_turn(
        config,
        agent_prompt_id,
        request,
        response_mode,
        &mut |state| {
            observe_attempt_state(&mut correlation, response_mode, on_update, state);
        },
    )? {
        VcrTurnSetup::Replayed(state) => return Ok(*state),
        VcrTurnSetup::Live { record_config } => record_config,
    };

    let session_id = request.session_id.as_str();
    let key = PoolKey::for_request(config, request);

    let pool_started = private_trace::started(private_trace);
    let checkout = pool.checkout_until(&key, &config.api_key, abort);
    if let (Some(trace), Some(started)) = (private_trace.as_mut(), pool_started) {
        trace.pool_wait_finished(started);
    }
    if let Some(conn) = checkout? {
        let turn_context = SharedTurnContext {
            pool,
            key: key.clone(),
            config,
            agent_prompt_id,
            request,
            record_config: record_config.as_ref(),
            response_mode,
        };
        match turn_context.run_cached(
            conn,
            session_id,
            &mut correlation,
            abort,
            on_update,
            private_trace,
        )? {
            CachedSharedTurn::Completed(state) => return Ok(*state),
            CachedSharedTurn::RetryFresh {
                response_bytes,
                stale_chain,
            } => {
                return SharedTurnContext {
                    pool,
                    key,
                    config,
                    agent_prompt_id,
                    request,
                    record_config: record_config.as_ref(),
                    response_mode,
                }
                .run_fresh(
                    response_bytes,
                    stale_chain,
                    false,
                    &mut correlation,
                    abort,
                    on_update,
                    private_trace,
                );
            }
        }
    }

    SharedTurnContext {
        pool,
        key,
        config,
        agent_prompt_id,
        request,
        record_config: record_config.as_ref(),
        response_mode,
    }
    .run_fresh(
        0,
        false,
        true,
        &mut correlation,
        abort,
        on_update,
        private_trace,
    )
}

impl<'a, 'request> SharedTurnContext<'a, 'request> {
    fn run_cached(
        self,
        mut conn: WsConn,
        session_id: &str,
        correlation: &mut Option<&mut crate::attempt_failure::AttemptCaptureCorrelation>,
        abort: &mut impl TurnAbort,
        on_update: &mut impl FnMut(crate::StreamUpdate<'_>),
        private_trace: &mut Option<private_trace::AttemptTrace>,
    ) -> Result<CachedSharedTurn, WsTurnError> {
        if abort.is_aborted() {
            self.pool.release(self.key, conn)?;
            return Err(WsTurnError::Canceled);
        }

        let mut stream = recording_stream(self.record_config);
        let mut observation = TurnObservation::default();
        let updates = path_std_cell::RefCell::new(on_update);
        let dispatch = correlation
            .as_deref_mut()
            .map(path_crate_attempt_failure::AttemptCaptureCorrelation::next_dispatch);
        match conn.run_response(
            self.config,
            self.agent_prompt_id,
            self.request,
            dispatch,
            stream.as_mut(),
            self.response_mode,
            abort,
            &mut |at| updates.borrow_mut()(crate::StreamUpdate::Dispatched(at)),
            &mut |state| {
                observation.observe(state);
                observe_attempt_state(
                    correlation,
                    self.response_mode,
                    &mut *updates.borrow_mut(),
                    state,
                );
            },
            private_trace,
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
            Err(err)
                if recovery_decision(&err, observation.semantic_progress)
                    == RecoveryDecision::Repair =>
            {
                let silent_reconnects = self.pool.bump_silent_reconnects()?;
                tracing::info!(
                    target: crate::LOG_TARGET,
                    session_id,
                    silent_reconnects,
                    "Codex WS connection lost mid-turn",
                );
                drop(conn);
                Ok(CachedSharedTurn::RetryFresh {
                    response_bytes: observation.response_bytes,
                    stale_chain: is_stale_chain_error(&err),
                })
            }
            Err(other) => {
                drop(conn);
                self.pool.abandon(&self.key)?;
                Err(WsTurnError::Other(other))
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "private observation follows turn ownership"
    )]
    fn run_fresh(
        self,
        carried_response_bytes: u64,
        stale_chain: bool,
        emit_dispatched: bool,
        correlation: &mut Option<&mut crate::attempt_failure::AttemptCaptureCorrelation>,
        abort: &mut impl TurnAbort,
        on_update: &mut impl FnMut(crate::StreamUpdate<'_>),
        private_trace: &mut Option<private_trace::AttemptTrace>,
    ) -> Result<crate::common::StreamState, WsTurnError> {
        if abort.is_aborted() {
            self.pool.abandon(&self.key)?;
            return Err(WsTurnError::Canceled);
        }
        if !emit_dispatched && let Some(correlation) = correlation.as_deref_mut() {
            correlation.mark_repair_used();
        }
        on_update(crate::StreamUpdate::Connecting);
        let connect_started = private_trace::started(private_trace);
        let connected = self.pool.connect_reserved_fresh(
            &self.key,
            self.config,
            abort,
            |config, thread_id, abort| {
                WsConn::connect(config, thread_id, &self.pool.network, abort)
            },
        );
        if let (Some(trace), Some(started)) = (private_trace.as_mut(), connect_started) {
            trace.connect_upgrade_finished(started);
        }
        let mut conn = connected?;
        self.pool.record_fresh_open()?;
        let dispatch = correlation
            .as_deref_mut()
            .map(path_crate_attempt_failure::AttemptCaptureCorrelation::next_dispatch);
        conn.carry_response_bytes(carried_response_bytes);
        let mut stream = recording_stream(self.record_config);
        let mut observation = TurnObservation::with_carried_response_bytes(carried_response_bytes);
        let updates = path_std_cell::RefCell::new(on_update);
        match conn.run_response(
            self.config,
            self.agent_prompt_id,
            self.request,
            dispatch,
            stream.as_mut(),
            self.response_mode,
            abort,
            &mut |at| {
                if emit_dispatched {
                    updates.borrow_mut()(crate::StreamUpdate::Dispatched(at));
                }
            },
            &mut |state| {
                observation.observe(state);
                observe_attempt_state(
                    correlation,
                    self.response_mode,
                    &mut *updates.borrow_mut(),
                    state,
                );
            },
            private_trace,
        ) {
            Ok(mut turn) => {
                turn.state.stale_chain_fallback = stale_chain;
                self.release_and_store_recording(conn, turn, stream)
            }
            Err(err)
                if self.record_config.is_none()
                    && repair_budget_available(emit_dispatched)
                    && recovery_decision(&err, observation.semantic_progress)
                        == RecoveryDecision::Repair =>
            {
                drop(conn);
                self.pool.bump_silent_reconnects()?;
                self.run_fresh(
                    observation.response_bytes,
                    stale_chain || is_stale_chain_error(&err),
                    false,
                    correlation,
                    abort,
                    updates.into_inner(),
                    private_trace,
                )
            }
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

fn repair_budget_available(first_dispatch: bool) -> bool {
    first_dispatch
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
        let mut abort = NeverAbort;
        match conn.run_prewarm(config, request, &mut abort, None) {
            Ok(state) => {
                pool.release(key, conn);
                return Ok(state);
            }
            Err(err) if is_recoverable_ws_error(&err) => {
                pool.stats.silent_reconnects += 1;
                tracing::info!(
                    target: crate::LOG_TARGET,
                    session_id,
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

    let mut abort = NeverAbort;
    let mut conn = WsConn::connect(
        config,
        &key.thread_id,
        &crate::test_network_policy(),
        &mut abort,
    )?;
    pool.stats.upgrades += 1;
    match conn.run_prewarm(config, request, &mut abort, None) {
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

/// Thread-safe prewarm entry point for a supervised blocking worker.
///
/// It reserves only the matching key while network work is in flight, so prompt
/// workers on other keys continue concurrently. A busy same key skips. The
/// caller-owned abort source covers connect, response wait, and a staged socket
/// release that cannot race cancellation; errors and unwind abandon the exact
/// reservation. Publication unregisters its callback and rechecks
/// [`TurnAbort::is_aborted`] before waking any same-key waiter.
pub fn run_prewarm_through_shared_pool(
    pool: &SharedWsPool,
    config: &ResponsesConfig,
    session_id: &str,
    request: &crate::common::PromptPayload<'_>,
    abort: &mut impl TurnAbort,
) -> Result<Option<crate::common::StreamState>, LlmError> {
    let key = PoolKey::for_request(config, request);

    let cached = if let TryCheckout::Reserved(cached) = pool
        .try_checkout(&key, &config.api_key)
        .map_err(WsTurnError::into_llm_error)?
    {
        cached
    } else {
        tracing::debug!(
            target: crate::LOG_TARGET,
            session_id,
            "skipping prompt prewarm: websocket pool key is busy",
        );
        return Ok(None);
    };
    let mut reservation = PrewarmReservation {
        pool,
        key: key.clone(),
        generation: pool
            .claim_prewarm(&key)
            .map_err(WsTurnError::into_llm_error)?,
        armed: true,
    };
    let cancel_pool = pool.clone();
    let cancel_key = key.clone();
    let cancel_generation = reservation.generation;
    let cancel_guard = abort.register_waker(Arc::new(move || {
        let _ = cancel_pool.invalidate_prewarm(&cancel_key, cancel_generation);
    }));

    if abort.is_aborted() {
        return Err(LlmError::Canceled);
    }

    let repair_after_fresh = cached.is_none();
    let mut deadline = None;
    if let Some(mut conn) = cached {
        let attempt_deadline = path_std_time::Instant::now() + super::ws::PREWARM_RESPONSE_TIMEOUT;
        deadline = Some(attempt_deadline);
        match conn.run_prewarm(config, request, abort, deadline) {
            Ok(state) => {
                if abort.is_aborted() {
                    return Err(LlmError::Canceled);
                }
                reservation
                    .publish(conn, cancel_guard, abort)
                    .map_err(WsTurnError::into_llm_error)?;
                return Ok(Some(state));
            }
            Err(err) if is_recoverable_ws_error(&err) => {
                pool.bump_silent_reconnects()
                    .map_err(WsTurnError::into_llm_error)?;
                tracing::info!(
                    target: crate::LOG_TARGET,
                    session_id,
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

    if deadline.is_some_and(|deadline| deadline <= path_std_time::Instant::now()) {
        return Err(LlmError::HttpStatus(
            0,
            "websocket prewarm response timeout".to_owned(),
        ));
    }
    let mut conn =
        match pool.connect_reserved_fresh(&key, config, abort, |config, thread_id, abort| {
            let timeout = deadline
                .map(|deadline| deadline.saturating_duration_since(path_std_time::Instant::now()))
                .unwrap_or(super::ws::CONNECT_TIMEOUT);
            WsConn::connect_with_timeout(config, thread_id, &pool.network, abort, timeout)
        }) {
            Ok(conn) => conn,
            Err(error) => return Err(error.into_llm_error()),
        };
    pool.record_fresh_open()
        .map_err(WsTurnError::into_llm_error)?;
    let deadline = deadline
        .or_else(|| Some(path_std_time::Instant::now() + super::ws::PREWARM_RESPONSE_TIMEOUT));
    match conn.run_prewarm(config, request, abort, deadline) {
        Ok(state) => {
            if abort.is_aborted() {
                return Err(LlmError::Canceled);
            }
            reservation
                .publish(conn, cancel_guard, abort)
                .map_err(WsTurnError::into_llm_error)?;
            Ok(Some(state))
        }
        Err(err) if repair_after_fresh && is_recoverable_ws_error(&err) => {
            drop(conn);
            pool.bump_silent_reconnects()
                .map_err(WsTurnError::into_llm_error)?;
            let deadline = deadline.expect("fresh prewarm establishes response deadline");
            if deadline <= path_std_time::Instant::now() {
                return Err(LlmError::HttpStatus(
                    0,
                    "websocket prewarm response timeout".to_owned(),
                ));
            }
            let mut replacement = pool
                .connect_reserved_fresh(&key, config, abort, |config, thread_id, abort| {
                    WsConn::connect_with_timeout(
                        config,
                        thread_id,
                        &pool.network,
                        abort,
                        deadline.saturating_duration_since(path_std_time::Instant::now()),
                    )
                })
                .map_err(WsTurnError::into_llm_error)?;
            pool.record_fresh_open()
                .map_err(WsTurnError::into_llm_error)?;
            let state = replacement.run_prewarm(config, request, abort, Some(deadline))?;
            reservation
                .publish(replacement, cancel_guard, abort)
                .map_err(WsTurnError::into_llm_error)?;
            Ok(Some(state))
        }
        Err(err) => {
            drop(conn);
            Err(err)
        }
    }
}

/// Errors from `WsConn::run_turn` that mean "this socket is dead,
/// but the *next* socket can probably serve the turn." Caller's job
/// is to reopen and retry once silently rather than letting the outer
/// retry loop burn a backoff on the same broken state.
///
/// Local close/task/send failures use `HttpStatus(0, "stream error: ...")`.
/// Structured provider error events use `StreamError` and preserve their
/// canonical code separately from untrusted prose:
///
/// - Transport-level closes: tungstenite raised `ConnectionClosed`,
///   `AlreadyClosed`, or an IO break; the server sent a close frame mid-stream;
///   WebSocket control ping or turn-send failed write-side.
/// - Task-supervision failures: the per-conn reader or writer task exited or
///   got aborted — the socket they owned is gone.
/// - Server-level stale-chain and connection-limit codes retire the socket and
///   may spend the same sole repair budget.
///
/// Local `"stream error:"` failures remain deliberately broad so new
/// dead-socket modes do not burn an outer backoff on a known-dead connection.
/// Structured codes, rather than message text, own provider-event precedence.
///
/// Carve-outs: account-level caps (usage_limit_reached, rate limit,
/// quota) reach us with the same prefix because they ride the same
/// `error` event, but the connection is fine — reopening just burns a
/// fresh upgrade against an upstream that's about to reject every
/// request the same way. Local provider-stream idle watchdog failures
/// also use the same prefix, but replaying them would keep a stuck turn
/// in-flight for another idle window. Defer those to the outer
/// classifier as a typed Transport retry without extending this attempt by a
/// second idle window.
fn is_recoverable_ws_error(err: &LlmError) -> bool {
    if matches!(err.root_error(), LlmError::WsClosed(_)) {
        return true;
    }
    if err.failure_kind().is_some() {
        return false;
    }
    if let Some(code) = err.stream_error_code() {
        return code == "websocket_connection_limit_reached"
            || [
                "previous_response_not_found",
                "previous_response_id_not_found",
                "invalid_previous_response_id",
            ]
            .contains(&code);
    }
    let body = match err.root_error() {
        LlmError::HttpStatus(0, body) => body,
        _ => return false,
    };
    if !body.starts_with("stream error:") {
        return false;
    }
    !crate::common::is_account_limit_body(body)
        && !crate::common::is_provider_stream_idle_timeout_body(body)
}

fn is_stale_chain_error(error: &LlmError) -> bool {
    let Some(code) = error.stream_error_code() else {
        return false;
    };
    [
        "previous_response_not_found",
        "previous_response_id_not_found",
        "invalid_previous_response_id",
    ]
    .contains(&code)
}

/// Closed decision for the sole in-attempt WebSocket repair budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RecoveryDecision {
    /// Discard the dead cached socket and spend the one fresh-socket repair.
    Repair,
    /// Surface the outcome without replaying semantic work.
    Surface,
}

pub(super) fn recovery_decision(error: &LlmError, semantic_progress: bool) -> RecoveryDecision {
    if !semantic_progress && is_recoverable_ws_error(error) {
        RecoveryDecision::Repair
    } else {
        RecoveryDecision::Surface
    }
}

#[cfg(test)]
mod tests;
