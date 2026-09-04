//! ChatGPT/Codex provider backend helpers.
//!
//! This crate owns the ChatGPT/Codex model metadata and OpenAI Responses API
//! implementation, including pooled WebSocket inference and HTTPS control-plane
//! operations.
//! Component boundaries and provider-visible replay are summarized in
//! `ARCH-tau-provider-codex` and
//! `SPEC-tau-provider-codex-streaming-replay`.

#[cfg(test)]
use std::collections as path_std_collections;
use std::collections::{HashMap, hash_map as path_std_collections_hash_map};
use std::num::NonZeroU32;
use std::sync as path_std_sync;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

mod compact_v2;
mod decoded_event;
use attempt_context::{AttemptOperation, ProviderAttemptContext, RetryFailureInput};
use compact_v2::build_v2_compacted_window;
use responses::pool as path_responses_pool;
use responses::ws::ResponseMode;
use tau_proto::{
    Effort, ModelId, ModelName, ModelTag, ProviderBackendTransport, ProviderModelInfo,
    ProviderName, ThinkingSummary, Verbosity,
};
use tau_provider::{
    debug_capture_writer as path_tau_provider_debug_capture_writer,
    private_attempt_trace as private_trace,
};

pub const LOG_TARGET: &str = "provider-codex";

/// ChatGPT/Codex backend base URL, without the final Responses path.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

const DEFAULT_RAW_CONTEXT_WINDOW: tau_proto::TokenCount = tau_proto::TokenCount::new(272_000);
const GPT_5_6_RAW_CONTEXT_WINDOW: tau_proto::TokenCount = tau_proto::TokenCount::new(372_000);
const GPT_5_6_STANDALONE_COMPACTION_TOKEN_THRESHOLD: tau_proto::TokenCount =
    tau_proto::TokenCount::new(334_800);
const EFFECTIVE_CONTEXT_WINDOW_PERCENT: u64 = 95;
const CHATGPT_MODELS: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex",
];

mod attempt_context;
mod attempt_failure;
mod canonical_identifier;
pub(crate) mod common;
pub mod oauth;
pub(crate) mod quota;
pub(crate) mod responses;

pub use attempt_failure::{LogicalAttempt, RedactedProviderDetail};
pub use common::{ProviderTokenCounts, StreamDeltaEmitter, StreamState};
pub use quota::{
    FullQuotaSnapshot, QuotaWindowObservation, RollingQuotaObservation, UsageFetchError,
};

/// Parses one synthetic WebSocket quota event for cross-crate tests.
#[cfg(feature = "test-support")]
pub fn parse_quota_ws_event(event: &str) -> Option<RollingQuotaObservation> {
    quota::parse_ws_event(event)
}

/// Constructs an empty synthetic stream state for cross-crate compatibility
/// tests.
#[cfg(feature = "test-support")]
#[must_use]
pub fn test_stream_state() -> StreamState {
    StreamState::new()
}

/// Constructs an empty opaque debug capture for cross-crate tests.
#[cfg(feature = "test-support")]
#[must_use]
pub fn test_debug_capture() -> CodexDebugCapture {
    CodexDebugCapture {
        terminal_event: None,
    }
}

/// Run synthetic provider prose through the production Codex redaction boundary
/// for cross-crate privacy tests.
#[cfg(feature = "test-support")]
#[must_use]
pub fn test_redacted_provider_detail(
    message: &str,
    access_token: &str,
    account_id: Option<&str>,
) -> Option<RedactedProviderDetail> {
    attempt_failure::AttemptFailureEvidence::provider(&serde_json::json!({
        "type": "error",
        "message": message,
    }))
    .live_detail(access_token, account_id)
}

/// Appends synthetic assistant text for cross-crate streaming tests.
#[cfg(feature = "test-support")]
pub fn test_append_message_delta(state: &mut StreamState, output_index: usize, delta: &str) {
    state.append_message_delta_at(output_index, delta);
}

/// Appends synthetic custom-tool input for cross-crate streaming tests.
#[cfg(feature = "test-support")]
pub fn test_append_custom_tool_input(state: &mut StreamState, output_index: usize, input: &str) {
    state
        .tool_call_at_mut(output_index, tau_proto::ToolType::Custom)
        .arguments_json
        .push_str(input);
}

/// Adds transport-only bytes to a synthetic stream state.
#[cfg(feature = "test-support")]
pub fn test_record_transport_response_bytes(state: &mut StreamState, bytes: usize) {
    state.record_transport_response_bytes(bytes);
}

/// Startup-resolved ChatGPT credentials used by one backend configuration.
///
/// This type intentionally has no `Debug` implementation so bearer material
/// cannot enter diagnostics through derived formatting.
pub struct ResolvedCredentials {
    /// OAuth bearer accepted by the Codex endpoint.
    access_token: String,
    /// Optional ChatGPT account selector paired with the bearer.
    account_id: Option<String>,
}

impl ResolvedCredentials {
    /// Creates one resolved credential generation.
    #[must_use]
    pub fn new(access_token: String, account_id: Option<String>) -> Self {
        Self {
            access_token,
            account_id,
        }
    }
}

/// Startup-stable Responses mode selected by the owning profile.
pub type CodexMode = responses::ResponsesMode;

/// Fully resolved, non-serialized configuration for one Codex operation.
///
/// Credential-bearing fields remain private and this type deliberately has no
/// `Debug` implementation.
#[derive(Clone)]
pub struct ResolvedConfig {
    /// Private wire configuration shared by inference and control-plane calls.
    inner: responses::ResponsesConfig,
}

/// Exact ChatGPT account identity retained across automatic prompt retries.
///
/// This proof deliberately contains no bearer token and has no `Debug` or
/// serialization implementation. It can only answer whether a newly resolved
/// configuration belongs to the originally accepted account.
#[derive(Clone, Eq, PartialEq)]
pub struct ChatGptRetryIdentity {
    /// Domain-separated digest of the accepted non-empty account selector.
    ///
    /// Absence is retained explicitly so a malformed initial identity cannot
    /// become authority to adopt a different account during retry.
    account_digest: Option<blake3::Hash>,
}

/// Opaque identity for quota/account control-plane state.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct QuotaProfileIdentity(u64);

#[cfg(feature = "test-support")]
impl QuotaProfileIdentity {
    /// Constructs a deterministic synthetic identity for coordinator tests.
    #[must_use]
    pub fn from_test_value(value: u64) -> Self {
        Self(value)
    }
}

#[cfg(feature = "test-support")]
impl From<u64> for QuotaProfileIdentity {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// Opaque identity for mode-sensitive inference and prewarm state.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InferenceProfileIdentity(u64);

#[cfg(feature = "test-support")]
impl InferenceProfileIdentity {
    /// Constructs a deterministic synthetic inference identity for tests.
    #[must_use]
    pub fn from_test_value(value: u64) -> Self {
        Self(value)
    }
}

impl ResolvedConfig {
    /// Returns the credential-free configured endpoint.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// Returns the startup-stable Responses mode.
    #[must_use]
    pub fn mode(&self) -> CodexMode {
        self.inner.mode
    }

    /// Returns the configured upstream model id.
    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.inner.model_id
    }

    /// Returns whether reasoning effort is supported.
    #[must_use]
    pub fn supports_reasoning_effort(&self) -> bool {
        self.inner.supports_reasoning_effort
    }

    /// Returns whether reasoning summaries are supported.
    #[must_use]
    pub fn supports_reasoning_summary(&self) -> bool {
        self.inner.supports_reasoning_summary
    }

    /// Returns whether verbosity is supported.
    #[must_use]
    pub fn supports_verbosity(&self) -> bool {
        self.inner.supports_verbosity
    }

    /// Returns whether assistant-message phase is supported.
    #[must_use]
    pub fn supports_phase(&self) -> bool {
        self.inner.supports_phase
    }

    /// Returns whether encrypted reasoning replay is supported.
    #[must_use]
    pub fn supports_encrypted_reasoning(&self) -> bool {
        self.inner.supports_encrypted_reasoning
    }

    /// Returns whether inline compaction is supported.
    #[must_use]
    pub fn supports_compaction(&self) -> bool {
        self.inner.supports_compaction
    }

    /// Returns whether prompt cache keys are supported.
    #[must_use]
    pub fn supports_prompt_cache_key(&self) -> bool {
        self.inner.supports_prompt_cache_key
    }

    /// Returns the raw provider context window.
    #[must_use]
    pub fn raw_context_window(&self) -> tau_proto::TokenCount {
        self.inner.raw_context_window
    }

    /// Returns whether a bearer credential is present.
    #[must_use]
    pub fn has_credential(&self) -> bool {
        !self.inner.api_key.trim().is_empty()
    }

    /// Returns whether an account selector is present.
    #[must_use]
    pub fn has_account_id(&self) -> bool {
        self.inner.account_id.is_some()
    }

    /// Compares resolved credentials without returning either secret value.
    #[must_use]
    pub fn credentials_match(&self, access_token: &str, account_id: Option<&str>) -> bool {
        self.inner.api_key == access_token && self.inner.account_id.as_deref() == account_id
    }

    /// Returns the minimum closed identity proof needed to pin automatic
    /// retries.
    #[must_use]
    pub fn chatgpt_retry_identity(&self) -> ChatGptRetryIdentity {
        let account_digest = self
            .inner
            .account_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|account_id| {
                let mut hasher = blake3::Hasher::new();
                hasher.update(b"tau.chatgpt.prompt-retry-account.v1\0");
                hasher.update(account_id.as_bytes());
                hasher.finalize()
            });
        ChatGptRetryIdentity { account_digest }
    }

    /// Checks whether this configuration belongs to an accepted retry identity.
    #[must_use]
    pub fn matches_chatgpt_retry_identity(&self, identity: &ChatGptRetryIdentity) -> bool {
        identity.account_digest.is_some() && self.chatgpt_retry_identity() == *identity
    }

    /// Returns an opaque process-local identity for endpoint and credential
    /// generation comparisons.
    #[must_use]
    pub fn profile_identity(&self) -> QuotaProfileIdentity {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = path_std_collections_hash_map::DefaultHasher::new();
        self.inner.base_url.hash(&mut hasher);
        self.inner.account_id.hash(&mut hasher);
        self.inner.api_key.hash(&mut hasher);
        QuotaProfileIdentity(hasher.finish())
    }

    /// Returns an opaque process-local identity for inference and prewarm
    /// state, including the startup-stable Responses mode.
    #[must_use]
    pub fn inference_identity(&self) -> InferenceProfileIdentity {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = path_std_collections_hash_map::DefaultHasher::new();
        self.profile_identity().0.hash(&mut hasher);
        self.inner.mode.is_lite_compatibility().hash(&mut hasher);
        InferenceProfileIdentity(hasher.finish())
    }

    fn wire(&self) -> &responses::ResponsesConfig {
        &self.inner
    }
}

/// Borrowed semantic prompt passed to one finite Codex operation.
pub type Prompt<'a> = common::PromptPayload<'a>;

/// Maximum number of concurrently supervised best-effort prewarms.
pub const MAX_CONCURRENT_PREWARMS: usize = responses::pool::DEFAULT_POOL_MAX;

/// Whether a failed finite attempt parsed replay-unsafe model output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticProgress {
    /// No model-semantic output was parsed; retry remains safe.
    None,
    /// Model-semantic output was parsed and any retry must first clear it.
    Parsed,
}

/// Sanitized typed failure from one finite Codex operation.
pub struct CodexError(common::LlmError);

impl CodexError {
    /// Returns the logical retry decision, when the outer scheduler should
    /// retry.
    #[must_use]
    pub fn retry_decision(&self) -> Option<tau_provider::retry_policy::RetryDecision> {
        self.0.retry_decision()
    }

    /// Returns the proven terminal provider category, when present.
    #[must_use]
    pub fn failure_kind(&self) -> Option<tau_proto::ProviderFailureKind> {
        self.0.failure_kind()
    }

    /// Returns repetition evidence for the dedicated terminal projection.
    #[must_use]
    pub fn repetition(&self) -> Option<&tau_provider::StreamRepetition> {
        match self.0.root_error() {
            common::LlmError::Observed { .. } => None,
            common::LlmError::RepetitionDetected(repetition) => Some(repetition),
            _ => None,
        }
    }

    /// Wraps backend repetition evidence as a terminal Codex failure.
    #[must_use]
    #[cfg(feature = "test-support")]
    pub fn from_repetition(repetition: tau_provider::StreamRepetition) -> Self {
        Self(common::LlmError::RepetitionDetected(repetition))
    }
}

impl std::fmt::Display for CodexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.root_error() {
            common::LlmError::Observed { .. } => formatter.write_str("provider request failed"),
            common::LlmError::Outbound(error) => error.fmt(formatter),
            common::LlmError::HttpStatus(status, _) => {
                write!(formatter, "provider request failed with HTTP {status}")
            }
            common::LlmError::HttpStatusRetryAfter(status, _, _) => {
                write!(formatter, "provider request failed with HTTP {status}")
            }
            common::LlmError::StreamError { .. } => {
                formatter.write_str("provider WebSocket stream failed")
            }
            common::LlmError::WsClosed(_) => formatter.write_str("provider WebSocket closed"),
            common::LlmError::Canceled => formatter.write_str("request canceled"),
            common::LlmError::ReloadableConfig(_) => {
                formatter.write_str("provider configuration must be reloaded")
            }
            common::LlmError::InvalidResponse(_) => {
                formatter.write_str("provider returned an invalid response")
            }
            common::LlmError::Io(_) => formatter.write_str("provider transport failed"),
            common::LlmError::Json(_) => {
                formatter.write_str("provider returned malformed structured data")
            }
            common::LlmError::Vcr(_) => formatter.write_str("provider replay failed"),
            common::LlmError::RepetitionDetected(repetition) => repetition.fmt(formatter),
            common::LlmError::WsUpgradeRequired => {
                formatter.write_str("Codex requires WebSocket; Tau has no HTTP/SSE fallback")
            }
            common::LlmError::ProviderFailure(kind, _) => {
                write!(formatter, "provider rejected the request ({kind:?})")
            }
        }
    }
}

impl std::fmt::Debug for CodexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CodexError(<redacted>)")
    }
}

impl std::error::Error for CodexError {}

/// Typed terminal from one finite Codex inference attempt.
pub enum AttemptOutcome {
    /// The provider completed a response.
    Finished(Box<StreamDispatchResult>),
    /// The logical scheduler may retry according to the typed decision.
    Retry {
        /// Provider-owned retry classification.
        decision: tau_provider::retry_policy::RetryDecision,
        /// Whether tentative semantic output must be cleared.
        progress: SemanticProgress,
        /// Bounded, redacted provider detail suitable only for live status.
        live_detail: Option<RedactedProviderDetail>,
        /// Whether the canonical provider response rejected this bearer with
        /// 401.
        canonical_unauthorized: bool,
    },
    /// Trusted local cancellation ended the attempt.
    Canceled {
        /// Whether tentative semantic output must be cleared.
        progress: SemanticProgress,
    },
    /// A proven terminal provider failure ended the attempt.
    Terminal {
        /// Sanitized typed backend error.
        error: CodexError,
        /// Whether tentative semantic output must be cleared.
        progress: SemanticProgress,
        /// Whether a request crossed the provider egress boundary.
        backend_reached: bool,
    },
}

/// Typed outcome of one finite best-effort prewarm operation.
pub enum PrewarmOutcome {
    /// A compatible response anchor was installed on the reserved socket.
    Installed,
    /// A real turn or another prewarm already owned the same socket key.
    SkippedBusy,
    /// The outer owner may retry according to the typed provider decision.
    Retry(tau_provider::retry_policy::RetryDecision),
    /// Trusted local cancellation ended the operation.
    Canceled,
    /// A proven terminal failure ended the operation.
    Terminal(CodexError),
}

/// Typed outcome of one finite joined standalone-compaction operation.
pub enum CompactOutcome {
    /// Provider returned an accepted replacement window.
    Finished {
        /// Provider-native replacement window.
        output_items: Vec<tau_proto::ContextItem>,
        /// Provider-reported usage for this compaction call, including cache
        /// reads and writes when present.
        usage: Option<tau_proto::ProviderTokenUsage>,
    },
    /// The outer scheduler may retry according to the typed decision.
    Retry {
        /// Provider-owned retry classification.
        decision: tau_provider::retry_policy::RetryDecision,
        /// Whether a request crossed the provider egress boundary.
        backend_reached: bool,
    },
    /// Trusted local cancellation ended the joined operation.
    Canceled {
        /// Whether a request crossed the provider egress boundary.
        backend_reached: bool,
    },
    /// A proven terminal failure ended the operation.
    Terminal {
        /// Sanitized typed backend error.
        error: CodexError,
        /// Whether a request crossed the provider egress boundary.
        backend_reached: bool,
    },
    /// The selected profile proved that its generic compact route is absent.
    RouteUnavailable {
        /// Sanitized terminal error for the current transaction.
        error: CodexError,
        /// Whether this attempt installed the process-local downgrade.
        newly_downgraded: bool,
        /// Exact resolved profile generation that proved unavailable.
        profile_identity: InferenceProfileIdentity,
        /// Whether a request crossed the provider egress boundary.
        backend_reached: bool,
    },
}

#[cfg(test)]
pub(crate) fn test_network_policy() -> tau_provider::OutboundNetworkPolicy {
    tau_provider::OutboundNetworkPolicy::from_environment(
        path_std_collections::BTreeMap::new(),
        None,
    )
}

/// Prompt-turn cancellation source used by the WebSocket transport.
///
/// The synchronous provider loop needs a cancellation event that can wake a
/// blocked WebSocket path. A turn may block while waiting for a same-key pool
/// reservation, a fresh DNS/TCP/TLS/WebSocket upgrade, or provider events on an
/// already-reserved socket. Implementors should register the supplied waker
/// with their native cancellation primitive and call it when the current turn
/// is canceled or the provider is shutting down.
pub trait TurnAbort {
    /// Return whether the current turn has already been canceled.
    fn is_aborted(&mut self) -> bool;

    /// Register a waker for future cancellation notifications.
    ///
    /// The returned guard must unregister the waker on drop so completed turns
    /// do not leave stale callbacks behind. Callers re-check
    /// [`Self::is_aborted`] after registration, so implementations need not
    /// notify registrations made after cancellation already became visible.
    fn register_waker(
        &mut self,
        waker: std::sync::Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker>;
}

/// Guard for a registered [`TurnAbort`] waker.
pub trait TurnAbortWaker: Send {}

/// Cancellation source that never aborts.
pub struct NeverAbort;

impl TurnAbort for NeverAbort {
    fn is_aborted(&mut self) -> bool {
        false
    }

    fn register_waker(
        &mut self,
        _waker: std::sync::Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker> {
        Box::new(NeverAbortWaker)
    }
}

struct NeverAbortWaker;

impl TurnAbortWaker for NeverAbortWaker {}

/// Runtime state for ChatGPT/Codex WebSocket inference and compaction
/// admission.
pub struct CodexRuntime {
    /// Shared pool whose connection setup uses `network`.
    ws_pool: responses::pool::SharedWsPool,
    /// Required immutable startup policy for all Codex control-plane and prompt
    /// traffic.
    network: std::sync::Arc<tau_provider::OutboundNetworkPolicy>,
    /// Generation-scoped v2 compaction admission observations.
    compact_routes: CompactAdmission,
}

/// Synchronized generation-scoped compaction route observations.
struct CompactAdmission {
    /// Current state by resolved profile generation.
    states: std::sync::Mutex<HashMap<InferenceProfileIdentity, CompactRouteObservation>>,
    /// Monotonic owner generation for distinguishing replacement probes of the
    /// same inference identity.
    next_probe_generation: AtomicU64,
    /// Wakes waiters after probe completion.
    changed: std::sync::Condvar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompactRouteState {
    Probing,
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompactRouteObservation {
    generation: u64,
    state: CompactRouteState,
}

enum CompactAdmissionResult<'a> {
    Probe(CompactProbe<'a>),
    Admitted,
    Unavailable,
    Canceled,
    InternalFailure,
}

struct CompactProbe<'a> {
    runtime: &'a CodexRuntime,
    identity: InferenceProfileIdentity,
    generation: u64,
    completed: bool,
}

impl CompactProbe<'_> {
    fn complete(mut self, state: CompactRouteState) -> bool {
        let committed = if let Ok(mut routes) = self.runtime.compact_routes.states.lock()
            && routes.get(&self.identity)
                == Some(&CompactRouteObservation {
                    generation: self.generation,
                    state: CompactRouteState::Probing,
                }) {
            routes.insert(
                self.identity,
                CompactRouteObservation {
                    generation: self.generation,
                    state,
                },
            );
            self.runtime.compact_routes.changed.notify_all();
            true
        } else {
            false
        };
        self.completed = true;
        committed
    }
}

impl Drop for CompactProbe<'_> {
    fn drop(&mut self) {
        if !self.completed
            && let Ok(mut routes) = self.runtime.compact_routes.states.lock()
            && routes.get(&self.identity)
                == Some(&CompactRouteObservation {
                    generation: self.generation,
                    state: CompactRouteState::Probing,
                })
        {
            routes.remove(&self.identity);
            self.runtime.compact_routes.changed.notify_all();
        }
    }
}

/// Result of one ChatGPT/Codex streaming dispatch.
pub struct StreamDispatchResult {
    /// Fully accumulated provider stream state.
    pub state: StreamState,
    /// WebSocket transport fact retained for the stable protocol descriptor.
    pub transport: ProviderBackendTransport,
    /// WebSocket pool counters changed by this turn, when available.
    pub ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    /// Opaque raw terminal capture retained only for backend-owned debug
    /// output.
    pub debug_capture: CodexDebugCapture,
}

/// Opaque Codex debug capture that never exposes raw provider JSON.
pub struct CodexDebugCapture {
    terminal_event: Option<serde_json::Value>,
}

/// Semantically disjoint live updates from one ChatGPT dispatch.
pub enum StreamUpdate<'a> {
    /// A fresh WebSocket upgrade is about to start. This is WebSocket-only and
    /// may occur more than once when a logical turn replaces a failed socket.
    Connecting,
    /// The first request is about to attempt transport enqueue. Emitted exactly
    /// once per logical attempt, including a failed enqueue, and suppressed for
    /// its sole transparent repair.
    Dispatched(std::time::Instant),
    /// A response-state observation for progress sampling. It may repeat
    /// unchanged during quiet waits.
    Response(&'a StreamState),
}

impl CodexRuntime {
    fn acquire_compact_probe(
        &self,
        identity: InferenceProfileIdentity,
        abort: &mut impl TurnAbort,
    ) -> CompactAdmissionResult<'_> {
        let Ok(mut routes) = self.compact_routes.states.lock() else {
            return CompactAdmissionResult::InternalFailure;
        };
        let mut waited_for_generation = None;
        loop {
            if abort.is_aborted() {
                return CompactAdmissionResult::Canceled;
            }
            let observation = routes.get(&identity).copied();
            if waited_for_generation.is_some()
                && observation.map(|observation| observation.generation) != waited_for_generation
            {
                return CompactAdmissionResult::Unavailable;
            }
            match observation.map(|observation| observation.state) {
                Some(CompactRouteState::Unavailable) => {
                    return CompactAdmissionResult::Unavailable;
                }
                Some(CompactRouteState::Probing) => {
                    waited_for_generation = observation.map(|observation| observation.generation);
                    let Ok(waited) = self
                        .compact_routes
                        .changed
                        .wait_timeout(routes, Duration::from_millis(25))
                    else {
                        return CompactAdmissionResult::InternalFailure;
                    };
                    routes = waited.0;
                }
                Some(CompactRouteState::Available) => return CompactAdmissionResult::Admitted,
                None => {
                    let generation = self
                        .compact_routes
                        .next_probe_generation
                        .fetch_add(1, Ordering::Relaxed);
                    routes.insert(
                        identity,
                        CompactRouteObservation {
                            generation,
                            state: CompactRouteState::Probing,
                        },
                    );
                    return CompactAdmissionResult::Probe(CompactProbe {
                        runtime: self,
                        identity,
                        generation,
                        completed: false,
                    });
                }
            }
        }
    }

    fn mark_compact_route_unavailable(&self, identity: InferenceProfileIdentity) -> bool {
        self.compact_routes
            .states
            .lock()
            .map(|mut routes| {
                let previous = routes.get(&identity).copied();
                if previous
                    .is_some_and(|observation| observation.state == CompactRouteState::Unavailable)
                {
                    return false;
                }
                let generation = previous.map_or_else(
                    || {
                        self.compact_routes
                            .next_probe_generation
                            .fetch_add(1, Ordering::Relaxed)
                    },
                    |observation| observation.generation,
                );
                routes.insert(
                    identity,
                    CompactRouteObservation {
                        generation,
                        state: CompactRouteState::Unavailable,
                    },
                );
                self.compact_routes.changed.notify_all();
                true
            })
            .unwrap_or(false)
    }

    /// Retires compaction admission state for one superseded
    /// credential/account generation.
    ///
    /// A matching in-flight probe loses completion authority, while a later
    /// probe for the same identity receives a distinct owner generation.
    pub fn retire_compact_identity(&self, identity: InferenceProfileIdentity) {
        if let Ok(mut routes) = self.compact_routes.states.lock()
            && routes.remove(&identity).is_some()
        {
            self.compact_routes.changed.notify_all();
        }
    }

    /// Create an empty Codex runtime using one immutable startup network
    /// policy.
    #[must_use]
    pub fn new(network: std::sync::Arc<tau_provider::OutboundNetworkPolicy>) -> Self {
        Self {
            ws_pool: path_responses_pool::SharedWsPool::new(path_std_sync::Arc::clone(&network)),
            network,
            compact_routes: CompactAdmission {
                states: path_std_sync::Mutex::new(HashMap::new()),
                next_probe_generation: AtomicU64::new(0),
                changed: path_std_sync::Condvar::new(),
            },
        }
    }

    /// Return the immutable outbound policy shared by this runtime.
    #[must_use]
    pub fn network(&self) -> &tau_provider::OutboundNetworkPolicy {
        &self.network
    }

    /// Clone the immutable outbound policy for supervised background work.
    #[must_use]
    pub fn network_arc(&self) -> std::sync::Arc<tau_provider::OutboundNetworkPolicy> {
        path_std_sync::Arc::clone(&self.network)
    }

    /// Stream one prompt through the Codex WebSocket transport.
    ///
    /// Retryable WebSocket failures are returned to the outer provider retry
    /// loop, which applies bounded backoff and eventually surfaces the
    /// terminal provider error for the turn. Known WS-capability or limit
    /// failures are surfaced directly instead of silently falling back to
    /// HTTP/SSE. The `abort` source is checked before starting WS work and
    /// is also registered as a wake source while a WS turn is blocked
    /// waiting for a same-key pool reservation, a fresh
    /// DNS/TCP/TLS/WebSocket upgrade, or provider events.
    /// Prompt cancellation and shutdown wake each wait; canceled turns return
    /// [`common::LlmError::Canceled`]. Fresh upgrades are independently bounded
    /// to 30 seconds.
    #[expect(
        clippy::too_many_arguments,
        reason = "transport parser mode joins existing lifecycle callbacks"
    )]
    fn stream(
        &self,
        agent_prompt_id: &str,
        config: &responses::ResponsesConfig,
        request: &Prompt<'_>,
        response_mode: ResponseMode,
        correlation: &mut attempt_failure::AttemptCaptureCorrelation,
        abort: &mut impl TurnAbort,
        on_update: &mut impl FnMut(StreamUpdate<'_>),
        private_trace: &mut Option<private_trace::AttemptTrace>,
    ) -> Result<StreamDispatchResult, common::LlmError> {
        let ws_pool_before = self.ws_pool.stats();
        let session_id = request.session_id.as_str();
        let dispatch = match response_mode {
            ResponseMode::Ordinary => responses::pool::run_turn_through_shared_pool_observed(
                &self.ws_pool,
                config,
                agent_prompt_id,
                request,
                Some(correlation),
                abort,
                on_update,
                private_trace,
            ),
            ResponseMode::Compact => responses::pool::run_compact_through_shared_pool_observed(
                &self.ws_pool,
                config,
                agent_prompt_id,
                request,
                Some(correlation),
                abort,
                on_update,
                private_trace,
            ),
        };
        let state = match dispatch {
            Ok(state) => state,
            Err(error) if is_ws_capability_or_limit_error(&error) => {
                let error = error.into_llm_error();
                tracing::warn!(
                    target: LOG_TARGET,
                    session_id,
                    "WS path failed with capability/limit error; surfacing without HTTP fallback",
                );
                return Err(error);
            }
            Err(other) => {
                let error = other.into_llm_error();
                tracing::warn!(
                    target: LOG_TARGET,
                    session_id,
                    "WS path failed; surfacing without HTTP fallback",
                );
                return Err(error);
            }
        };
        let ws_pool_delta = ws_pool_before.and_then(|before| {
            self.ws_pool
                .stats()
                .map(|after| compute_ws_pool_delta(before, after))
        });
        let mut state = state;
        let debug_capture = CodexDebugCapture {
            terminal_event: state.provider_terminal_event.take(),
        };
        Ok(StreamDispatchResult {
            state,
            transport: ProviderBackendTransport::Websocket,
            ws_pool_delta,
            debug_capture,
        })
    }

    /// Executes one finite Codex inference attempt and returns a typed
    /// scheduler outcome without writing harness events or sleeping for
    /// logical retry.
    pub fn run_attempt(
        &self,
        agent_prompt_id: &str,
        config: &ResolvedConfig,
        request: &Prompt<'_>,
        abort: &mut impl TurnAbort,
        on_update: &mut impl FnMut(StreamUpdate<'_>),
    ) -> AttemptOutcome {
        self.run_attempt_numbered(
            agent_prompt_id,
            LogicalAttempt::new(1),
            config,
            request,
            abort,
            on_update,
        )
    }

    /// Execute one numbered finite attempt and submit its private failure
    /// diagnostic without changing provider execution.
    pub fn run_attempt_numbered(
        &self,
        agent_prompt_id: &str,
        logical_attempt: LogicalAttempt,
        config: &ResolvedConfig,
        request: &Prompt<'_>,
        abort: &mut impl TurnAbort,
        on_update: &mut impl FnMut(StreamUpdate<'_>),
    ) -> AttemptOutcome {
        let mut private_trace = private_trace::AttemptTrace::selected(
            private_trace::Backend::Codex,
            private_trace::Transport::Websocket,
        );
        let mut attempt = ProviderAttemptContext::new(AttemptOperation::Inference, logical_attempt);
        let mut backend_reached = false;
        let result = self.stream(
            agent_prompt_id,
            config.wire(),
            request,
            ResponseMode::Ordinary,
            attempt.correlation(),
            abort,
            &mut |update| {
                if matches!(update, StreamUpdate::Dispatched(_)) {
                    backend_reached = true;
                }
                on_update(update);
            },
            &mut private_trace,
        );
        if let Ok(result) = &result {
            attempt.observe_stream(&result.state);
        }
        let progress = attempt.progress();
        let canceled = abort.is_aborted();
        if let Some(trace) = private_trace.take() {
            let trace_outcome = match &result {
                _ if canceled => private_trace::Outcome::Canceled,
                Ok(_) => private_trace::Outcome::Completed,
                Err(common::LlmError::Canceled) => private_trace::Outcome::Canceled,
                Err(error) if error.retry_decision().is_some() => private_trace::Outcome::Retryable,
                Err(_) => private_trace::Outcome::Failed,
            };
            trace.finish(trace_outcome);
        }
        if canceled {
            return AttemptOutcome::Canceled { progress };
        }
        match result {
            Ok(result) => AttemptOutcome::Finished(Box::new(result)),
            Err(common::LlmError::Canceled) => AttemptOutcome::Canceled { progress },
            Err(error) => match error.retry_decision() {
                Some(decision) => {
                    let canonical_unauthorized = error.is_canonical_unauthorized();
                    let live_detail = error.evidence().and_then(|evidence| {
                        evidence.live_detail(
                            config.wire().api_key.as_str(),
                            config.wire().account_id.as_deref(),
                        )
                    });
                    attempt.finalize_retry_failure(RetryFailureInput {
                        agent_prompt_id,
                        request,
                        decision: &decision,
                        evidence: error.evidence(),
                        access_token: config.wire().api_key.as_str(),
                        account_id: config.wire().account_id.as_deref(),
                    });
                    AttemptOutcome::Retry {
                        decision,
                        progress,
                        live_detail,
                        canonical_unauthorized,
                    }
                }
                None => AttemptOutcome::Terminal {
                    error: CodexError(error),
                    progress,
                    backend_reached,
                },
            },
        }
    }

    /// Best-effort non-generating prewarm for a later ChatGPT prompt.
    ///
    /// The caller must run this on supervised blocking work rather than its
    /// event loop. `abort` owns cancellation of connection, response wait, and
    /// socket installation. A fresh upgrade is bounded to 30 seconds and the
    /// response has a separate 30-second absolute bound. Success retains the
    /// socket and response-id anchor only for a real prompt with the exact
    /// non-input fingerprint and a lowered input extending the warmed prefix.
    /// A mismatch consumes the anchor and sends full context. Failure leaves no
    /// reservation or cached replacement behind. Socket publication unregisters
    /// its abort callback and then rechecks [`TurnAbort::is_aborted`]
    /// before releasing the same-key reservation, so custom abort
    /// implementations cannot publish cancellation that was already
    /// authoritative at callback retirement.
    pub fn prewarm(
        &self,
        config: &ResolvedConfig,
        session_id: &str,
        request: &Prompt<'_>,
        abort: &mut impl TurnAbort,
    ) -> PrewarmOutcome {
        match responses::pool::run_prewarm_through_shared_pool(
            &self.ws_pool,
            config.wire(),
            session_id,
            request,
            abort,
        ) {
            Ok(Some(_)) => PrewarmOutcome::Installed,
            Ok(None) => PrewarmOutcome::SkippedBusy,
            Err(common::LlmError::Canceled) => PrewarmOutcome::Canceled,
            Err(error) => match error.retry_decision() {
                Some(decision) => PrewarmOutcome::Retry(decision),
                None => PrewarmOutcome::Terminal(CodexError(error)),
            },
        }
    }

    /// Invalidates cached sockets and prevents currently reserved sockets from
    /// being reinstalled after a provider profile or session boundary.
    pub fn invalidate_all_websockets(&self) -> Result<(), CodexError> {
        self.ws_pool
            .invalidate_all()
            .map_err(path_responses_pool::WsTurnError::into_llm_error)
            .map_err(CodexError)
    }

    /// Invalidates only sockets and in-flight publications owned by one profile
    /// namespace, preserving unrelated configured providers.
    pub fn invalidate_profile_websockets(&self, provider: &ProviderName) -> Result<(), CodexError> {
        self.ws_pool
            .invalidate_profile(provider)
            .map_err(path_responses_pool::WsTurnError::into_llm_error)
            .map_err(CodexError)
    }

    /// Invalidates the matching chain, then runs one v2 WebSocket compaction.
    pub fn compact(
        &self,
        agent_prompt_id: &str,
        config: &ResolvedConfig,
        request: &Prompt<'_>,
        abort: &mut impl TurnAbort,
    ) -> CompactOutcome {
        self.compact_numbered(
            agent_prompt_id,
            LogicalAttempt::new(1),
            config,
            request,
            abort,
        )
    }

    /// Run one numbered native standalone-compaction attempt.
    pub fn compact_numbered(
        &self,
        agent_prompt_id: &str,
        logical_attempt: LogicalAttempt,
        config: &ResolvedConfig,
        request: &Prompt<'_>,
        abort: &mut impl TurnAbort,
    ) -> CompactOutcome {
        let identity = config.inference_identity();
        let probe = match self.acquire_compact_probe(identity, abort) {
            CompactAdmissionResult::Probe(probe) => Some(probe),
            CompactAdmissionResult::Admitted => None,
            CompactAdmissionResult::Canceled => {
                return CompactOutcome::Canceled {
                    backend_reached: false,
                };
            }
            CompactAdmissionResult::InternalFailure => {
                return CompactOutcome::Terminal {
                    error: CodexError(common::LlmError::InvalidResponse(
                        "compaction admission state unavailable".to_owned(),
                    )),
                    backend_reached: false,
                };
            }
            CompactAdmissionResult::Unavailable => {
                return CompactOutcome::RouteUnavailable {
                    error: CodexError(common::LlmError::ProviderFailure(
                        tau_proto::ProviderFailureKind::RequestRejected,
                        "standalone compaction route is unavailable for this profile".to_owned(),
                    )),
                    newly_downgraded: false,
                    profile_identity: identity,
                    backend_reached: false,
                };
            }
        };
        if let Err(error) = self.ws_pool.invalidate(config.wire(), request) {
            return if abort.is_aborted() {
                CompactOutcome::Canceled {
                    backend_reached: false,
                }
            } else {
                CompactOutcome::Terminal {
                    error: CodexError(error.into_llm_error()),
                    backend_reached: false,
                }
            };
        }
        if abort.is_aborted() {
            return CompactOutcome::Canceled {
                backend_reached: false,
            };
        }
        let mut attempt = ProviderAttemptContext::new(AttemptOperation::Compact, logical_attempt);
        let mut backend_reached = false;
        let mut private_trace = private_trace::AttemptTrace::selected(
            private_trace::Backend::Codex,
            private_trace::Transport::Websocket,
        );
        let compact_result = self.stream(
            agent_prompt_id,
            config.wire(),
            request,
            ResponseMode::Compact,
            attempt.correlation(),
            abort,
            &mut |update| {
                if matches!(update, StreamUpdate::Dispatched(_)) {
                    backend_reached = true;
                }
            },
            &mut private_trace,
        );
        let (state, usage) = match compact_result {
            Ok(dispatch) => {
                if let Some(probe) = probe {
                    probe.complete(CompactRouteState::Available);
                }
                let usage = dispatch.state.usage();
                (dispatch.state, usage)
            }
            Err(common::LlmError::Canceled) => {
                if let Some(trace) = private_trace.take() {
                    trace.finish(private_trace::Outcome::Canceled);
                }
                return CompactOutcome::Canceled { backend_reached };
            }
            Err(error) => {
                if error.is_compaction_route_unavailable() {
                    let newly_downgraded = if let Some(probe) = probe {
                        probe.complete(CompactRouteState::Unavailable)
                    } else {
                        self.mark_compact_route_unavailable(identity)
                    };
                    if let Some(trace) = private_trace.take() {
                        trace.finish(private_trace::Outcome::Failed);
                    }
                    return CompactOutcome::RouteUnavailable {
                        error: CodexError(error),
                        newly_downgraded,
                        profile_identity: identity,
                        backend_reached,
                    };
                }
                if let Some(probe) = probe {
                    probe.complete(CompactRouteState::Available);
                }
                let outcome = match (error.retry_decision(), attempt.progress()) {
                    (Some(decision), SemanticProgress::None) => {
                        attempt.finalize_retry_failure(RetryFailureInput {
                            agent_prompt_id,
                            request,
                            decision: &decision,
                            evidence: error.evidence(),
                            access_token: config.wire().api_key.as_str(),
                            account_id: config.wire().account_id.as_deref(),
                        });
                        CompactOutcome::Retry {
                            decision,
                            backend_reached,
                        }
                    }
                    _ => CompactOutcome::Terminal {
                        error: CodexError(error),
                        backend_reached,
                    },
                };
                if let Some(trace) = private_trace.take() {
                    let class = if matches!(outcome, CompactOutcome::Retry { .. }) {
                        private_trace::Outcome::Retryable
                    } else {
                        private_trace::Outcome::Failed
                    };
                    trace.finish(class);
                }
                return outcome;
            }
        };
        let Some(compaction_item) = state.into_single_compaction_item() else {
            if let Some(trace) = private_trace.take() {
                trace.finish(private_trace::Outcome::Failed);
            }
            return CompactOutcome::Terminal {
                error: CodexError(common::LlmError::InvalidResponse(
                    "compaction response did not contain exactly one canonical compaction item"
                        .to_owned(),
                )),
                backend_reached,
            };
        };
        let output = build_v2_compacted_window(
            request.context,
            vec![tau_proto::ContextItem::Compaction(compaction_item)],
        );
        if abort.is_aborted() {
            if let Some(trace) = private_trace.take() {
                trace.finish(private_trace::Outcome::Canceled);
            }
            CompactOutcome::Canceled { backend_reached }
        } else {
            if let Some(trace) = private_trace.take() {
                trace.finish(private_trace::Outcome::Completed);
            }
            CompactOutcome::Finished {
                output_items: output,
                usage,
            }
        }
    }

    /// Fetches one typed full account-quota snapshot with this runtime's
    /// immutable outbound policy.
    pub fn fetch_usage(
        &self,
        config: &ResolvedConfig,
    ) -> Result<FullQuotaSnapshot, UsageFetchError> {
        quota::fetch_usage(
            &config.inner.base_url,
            &config.inner.api_key,
            config.inner.account_id.as_deref(),
            &self.network,
        )
    }
}

/// Serialize and submit one explicitly permitted Codex response debug record
/// while keeping
/// raw provider JSON inside the backend boundary.
///
/// Return does not imply persistence. Compression and filesystem work run on a
/// bounded detached worker. Queue
/// overload, worker failure, and process shutdown may omit this best-effort
/// diagnostic without delaying or failing provider protocol work.
pub fn submit_response_debug(
    session_id: &tau_proto::SessionId,
    enabled: bool,
    response: &tau_proto::ProviderResponseFinished,
    capture: Option<&CodexDebugCapture>,
) {
    submit_response_debug_with(
        session_id,
        enabled,
        response,
        capture,
        tau_provider::debug_capture_writer::submit_provider_debug_capture,
    );
}

fn submit_response_debug_with(
    session_id: &tau_proto::SessionId,
    enabled: bool,
    response: &tau_proto::ProviderResponseFinished,
    capture: Option<&CodexDebugCapture>,
    submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
) {
    if !enabled {
        return;
    }
    let transport = response
        .backend
        .as_ref()
        .map(|backend| match backend.transport {
            ProviderBackendTransport::HttpSse => {
                path_tau_provider_debug_capture_writer::ProviderDebugCaptureClass::HttpSseResponse
            }
            ProviderBackendTransport::Websocket => {
                path_tau_provider_debug_capture_writer::ProviderDebugCaptureClass::WebsocketResponse
            }
        })
        .unwrap_or(
            path_tau_provider_debug_capture_writer::ProviderDebugCaptureClass::UnknownResponse,
        );
    let metadata = serde_json::json!({
        "session_id": session_id,
        "agent_prompt_id": response.agent_prompt_id,
        "backend": response.backend,
        "provider_response_id": response.provider_response_id,
        "usage": response.usage,
        "provider_response_finished": response,
        "provider_terminal_event": capture.and_then(|capture| capture.terminal_event.as_ref()),
    });
    match serde_json::to_vec_pretty(&metadata) {
        Ok(json) => submit(
            path_tau_provider_debug_capture_writer::ProviderDebugCapture::new(
                session_id.clone(),
                response.agent_prompt_id.clone(),
                transport,
                json,
            ),
        ),
        Err(error) => {
            tracing::warn!(target: LOG_TARGET, "failed to serialize provider response debug record: {error}");
        }
    }
}

fn is_ws_capability_or_limit_error(error: &responses::pool::WsTurnError) -> bool {
    match error {
        path_responses_pool::WsTurnError::Canceled => false,
        path_responses_pool::WsTurnError::Other(error) => {
            is_ws_capability_or_limit_llm_error(error)
        }
    }
}

fn is_ws_capability_or_limit_llm_error(error: &common::LlmError) -> bool {
    match error {
        common::LlmError::WsUpgradeRequired => true,
        common::LlmError::HttpStatus(426, _) => true,
        common::LlmError::HttpStatus(_, body) => {
            body.contains("websocket_connection_limit_reached")
        }
        _ => false,
    }
}

fn compute_ws_pool_delta(
    before: responses::pool::WsPoolStats,
    after: responses::pool::WsPoolStats,
) -> tau_proto::WsPoolDelta {
    let sub = |a: u64, b: u64| u32::try_from(a.saturating_sub(b)).unwrap_or(u32::MAX);
    tau_proto::WsPoolDelta {
        upgrades: sub(after.upgrades, before.upgrades),
        silent_reconnects: sub(after.silent_reconnects, before.silent_reconnects),
    }
}

/// Returns the hardcoded model publication records for one ChatGPT account.
#[must_use]
pub fn models_for_provider(provider: &ProviderName) -> Vec<ProviderModelInfo> {
    models_for_provider_mode(provider, responses::ResponsesMode::Standard)
}

/// Returns model publication records for one startup-selected ChatGPT profile.
#[must_use]
pub fn models_for_provider_mode(
    provider: &ProviderName,
    mode: CodexMode,
) -> Vec<ProviderModelInfo> {
    CHATGPT_MODELS
        .iter()
        .map(|model| model_info(provider, model, effective_mode(model, mode)))
        .collect()
}

/// Returns a Responses backend config for one ChatGPT/Codex model.
#[must_use]
#[cfg(test)]
pub(crate) fn config_for_model(
    model: &ModelName,
    access_token: String,
    account_id: Option<String>,
) -> responses::ResponsesConfig {
    config_for_model_mode(
        model,
        access_token,
        account_id,
        responses::ResponsesMode::Standard,
    )
}

/// Returns a Responses backend config for a startup-selected profile mode.
#[must_use]
#[cfg(test)]
pub(crate) fn config_for_model_mode(
    model: &ModelName,
    access_token: String,
    account_id: Option<String>,
    requested_mode: responses::ResponsesMode,
) -> responses::ResponsesConfig {
    resolved_config_for_model(
        model,
        ResolvedCredentials::new(access_token, account_id),
        requested_mode,
    )
    .inner
}

/// Resolves one model configuration from an explicit credential generation and
/// startup-stable mode.
#[must_use]
pub fn resolved_config_for_model(
    model: &ModelName,
    credentials: ResolvedCredentials,
    requested_mode: CodexMode,
) -> ResolvedConfig {
    resolved_config_for_provider_model(
        &ProviderName::new("chatgpt"),
        model,
        credentials,
        requested_mode,
    )
}

/// Resolves one namespaced model configuration from an explicit credential
/// generation and startup-stable mode.
#[must_use]
pub fn resolved_config_for_provider_model(
    provider: &ProviderName,
    model: &ModelName,
    credentials: ResolvedCredentials,
    requested_mode: CodexMode,
) -> ResolvedConfig {
    let model_id = model.as_str();
    let mode = effective_mode(model_id, requested_mode);
    ResolvedConfig {
        inner: responses::ResponsesConfig {
            profile_namespace: provider.clone(),
            mode,
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key: credentials.access_token,
            model_id: model_id.to_owned(),
            raw_context_window: raw_context_window_for_model(model_id),
            account_id: credentials.account_id,
            supports_reasoning_effort: true,
            supports_reasoning_summary: true,
            supports_verbosity: model_id.starts_with("gpt-5"),
            supports_phase: is_known_phase_capable_model_id(model_id),
            supports_encrypted_reasoning: true,
            supports_compaction: !is_gpt_5_6(model_id),
            supports_prompt_cache_key: true,
        },
    }
}

fn model_info(
    provider: &ProviderName,
    model: &str,
    mode: responses::ResponsesMode,
) -> ProviderModelInfo {
    let prices = estimated_api_prices(model);
    ProviderModelInfo {
        id: ModelId::new(provider.clone(), ModelName::new(model)),
        display_name: None,
        tags: vec![
            ModelTag::new("shell:chatgpt"),
            ModelTag::new("tools:custom-text"),
        ],
        hosted_tool_capabilities: (!mode.is_lite_compatibility())
            .then_some(tau_proto::ProviderHostedToolCapability::WebSearch {
                access_modes: vec![
                    tau_proto::ProviderWebSearchAccess::Cached,
                    tau_proto::ProviderWebSearchAccess::Live,
                ],
                supports_allowed_domains: true,
                supports_context_size: true,
            })
            .into_iter()
            .collect(),
        supported_tool_types: vec![tau_proto::ToolType::Function, tau_proto::ToolType::Custom],
        input_modalities: if is_gpt_5_6(model) {
            vec![
                tau_proto::InputModality::Text,
                tau_proto::InputModality::Image,
            ]
        } else {
            vec![tau_proto::InputModality::Text]
        },
        tool_result_modalities: if is_gpt_5_6(model) {
            vec![
                tau_proto::InputModality::Text,
                tau_proto::InputModality::Image,
            ]
        } else {
            vec![tau_proto::InputModality::Text]
        },
        supports_parallel_tool_calls: !mode.is_lite_compatibility(),
        default_affinity: default_affinity_for_model(model),
        context_window: raw_context_window_for_model(model),
        max_input_tokens: Some(effective_context_window_for_model(model)),
        max_output_tokens: None,
        efforts: efforts_for_model(model),
        verbosities: verbosities_for_model(model),
        thinking_summaries: vec![
            ThinkingSummary::Off,
            ThinkingSummary::Auto,
            ThinkingSummary::Concise,
            ThinkingSummary::Detailed,
        ],
        supports_compaction: !is_gpt_5_6(model),
        supports_standalone_compaction: is_gpt_5_6(model),
        standalone_compaction_generation_negative: false,
        standalone_compaction_threshold: is_gpt_5_6(model)
            .then_some(GPT_5_6_STANDALONE_COMPACTION_TOKEN_THRESHOLD),
        standalone_compaction_prefix_budget: None,
        cache_policy: Some(private_response_chain_cache_policy()),
        est_uncached_input_cost_1m_usd: Some(prices.uncached_input),
        est_cached_input_cost_1m_usd: None,
        est_cache_write_input_cost_1m_usd: None,
        est_output_cost_1m_usd: Some(prices.output),
        est_cache_storage_cost_1m_token_hour_usd: None,
    }
}

/// Return the conservative documented cache contract for the private
/// ChatGPT/Codex response-chain route.
fn private_response_chain_cache_policy() -> tau_proto::ProviderCachePolicy {
    tau_proto::ProviderCachePolicy {
        kind: tau_proto::ProviderCacheKind::ResponseChain,
        ttl: tau_proto::ProviderCacheTtl::Unknown,
        renewal: tau_proto::ProviderCacheRenewal::Recreate,
        output_floor: tau_proto::ProviderCacheOutputFloor::Zero,
        quota: tau_proto::ProviderCacheQuotaAccounting {
            requests: tau_proto::ProviderCacheQuotaCharge::Unknown,
            read_tokens: tau_proto::ProviderCacheQuotaCharge::Unknown,
            write_tokens: tau_proto::ProviderCacheQuotaCharge::Unknown,
            output_tokens: tau_proto::ProviderCacheQuotaCharge::Unknown,
        },
        prefix_identity_version: NonZeroU32::new(1).expect("one is nonzero"),
        privacy: tau_proto::ProviderCachePrivacy {
            storage: tau_proto::ProviderCacheStorageMode::Unknown,
            zero_data_retention: tau_proto::ProviderCacheZeroDataRetentionCompatibility::Unknown,
            data_residency: tau_proto::ProviderCacheDataResidencyEffect::Unknown,
            manual_deletion: tau_proto::ProviderCacheDeletionAvailability::Unavailable,
        },
    }
}

/// Basic standard-processing API prices from OpenAI's provider-owned table:
/// <https://developers.openai.com/api/docs/pricing>.
///
/// Tau deliberately ignores tiers, batch discounts, and service variants.
/// These values are estimates for equivalent API use even when the
/// actual ChatGPT route is subscription-backed.
fn estimated_api_prices(model: &str) -> tau_proto::EstimatedApiCostRates {
    use tau_proto::{EstimatedApiCostRates, EstimatedUsdPerMillion as Price};

    let (uncached, cached, output) = match model {
        "gpt-5.6-sol" | "gpt-5.5" => (5_000_000, 500_000, 30_000_000),
        "gpt-5.6-terra" => (2_000_000, 200_000, 12_000_000),
        "gpt-5.6-luna" => (200_000, 20_000, 1_200_000),
        "gpt-5.4" => (2_500_000, 250_000, 15_000_000),
        "gpt-5.4-mini" => (750_000, 75_000, 4_500_000),
        "gpt-5.3-codex" => (1_750_000, 175_000, 14_000_000),
        _ => return tau_proto::ESTIMATED_API_COST_FALLBACK,
    };
    EstimatedApiCostRates {
        uncached_input: Price::from_micro_usd(uncached),
        cached_input: Price::from_micro_usd(cached),
        cache_write_input: model
            .starts_with("gpt-5.6")
            .then(|| Price::from_micro_usd(uncached.saturating_mul(5) / 4)),
        output: Price::from_micro_usd(output),
        storage_per_million_token_hour: None,
    }
}

fn default_affinity_for_model(model: &str) -> i32 {
    match model {
        "gpt-5.6-sol" => 700,
        "gpt-5.6-terra" => 600,
        "gpt-5.6-luna" => 500,
        "gpt-5.5" => 400,
        "gpt-5.4" => 300,
        "gpt-5.3-codex" => 200,
        "gpt-5.4-mini" => 100,
        _ => 0,
    }
}

fn raw_context_window_for_model(model: &str) -> tau_proto::TokenCount {
    if is_gpt_5_6(model) {
        GPT_5_6_RAW_CONTEXT_WINDOW
    } else {
        DEFAULT_RAW_CONTEXT_WINDOW
    }
}

fn effective_context_window_for_model(model: &str) -> tau_proto::TokenCount {
    tau_proto::TokenCount::new(
        raw_context_window_for_model(model).get() * EFFECTIVE_CONTEXT_WINDOW_PERCENT / 100,
    )
}

fn effective_mode(model: &str, requested: responses::ResponsesMode) -> responses::ResponsesMode {
    if is_gpt_5_6(model) {
        requested
    } else {
        responses::ResponsesMode::Standard
    }
}

fn is_gpt_5_6(model: &str) -> bool {
    matches!(model, "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna")
}

fn efforts_for_model(model: &str) -> Vec<Effort> {
    let mut efforts = vec![
        Effort::Off,
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
    ];
    if supports_xhigh(model) {
        efforts.push(Effort::XHigh);
    }
    if is_gpt_5_6(model) {
        efforts.push(Effort::Max);
    }
    efforts
}

fn supports_xhigh(model: &str) -> bool {
    if model.contains("mini") || model.contains("nano") {
        return false;
    }
    [
        "gpt-5.5",
        "gpt-5.6",
        "gpt-5.4",
        "gpt-5.3-codex",
        "gpt-5.2",
        "gpt-5.1-codex-max",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix))
}

fn verbosities_for_model(model: &str) -> Vec<Verbosity> {
    if model.starts_with("gpt-5") {
        vec![Verbosity::Low, Verbosity::Medium, Verbosity::High]
    } else {
        vec![Verbosity::Medium]
    }
}

fn is_known_phase_capable_model_id(model_id: &str) -> bool {
    let trimmed = model_id.trim();
    let Some(rest) = trimmed.strip_prefix("gpt-5.") else {
        return false;
    };
    let (minor, suffix) = rest.split_once('-').unwrap_or((rest, ""));
    let Ok(n) = minor.parse::<u32>() else {
        return false;
    };

    4 <= n || (n == 3 && suffix.starts_with("codex"))
}

#[cfg(test)]
mod tests;
