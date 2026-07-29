//! ChatGPT/Codex provider backend helpers.
//!
//! This crate owns the ChatGPT/Codex model metadata and OpenAI Responses API
//! implementation, including pooled WebSocket inference and supported unary
//! HTTPS control-plane operations.
//! Component boundaries and provider-visible replay are summarized in
//! `ARCH-tau-provider-codex` and
//! `SPEC-tau-provider-codex-streaming-replay`.

use tau_proto::{
    Effort, ModelId, ModelName, ModelTag, ProviderBackendTransport, ProviderModelInfo,
    ProviderName, ThinkingSummary, Verbosity,
};

pub const LOG_TARGET: &str = "provider-codex";

/// ChatGPT/Codex backend base URL, without the final Responses path.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

const DEFAULT_RAW_CONTEXT_WINDOW: u64 = 272_000;
const GPT_5_6_RAW_CONTEXT_WINDOW: u64 = 372_000;
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

pub(crate) mod common;
pub mod oauth;
pub(crate) mod quota;
pub(crate) mod responses;

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
    pub fn raw_context_window(&self) -> u64 {
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

    /// Returns an opaque process-local identity for endpoint and credential
    /// generation comparisons.
    #[must_use]
    pub fn profile_identity(&self) -> QuotaProfileIdentity {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
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
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
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
        match &self.0 {
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
        match &self.0 {
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
    Finished(Vec<tau_proto::ContextItem>),
    /// The outer scheduler may retry according to the typed decision.
    Retry(tau_provider::retry_policy::RetryDecision),
    /// Trusted local cancellation ended the joined operation.
    Canceled,
    /// A proven terminal failure ended the operation.
    Terminal(CodexError),
}

#[cfg(test)]
pub(crate) fn test_network_policy() -> tau_provider::OutboundNetworkPolicy {
    tau_provider::OutboundNetworkPolicy::from_environment(std::collections::BTreeMap::new(), None)
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

/// Runtime state for the ChatGPT/Codex WebSocket inference pool.
pub struct CodexRuntime {
    /// Shared pool whose connection setup uses `network`.
    ws_pool: responses::pool::SharedWsPool,
    /// Required immutable startup policy for all Codex control-plane and prompt
    /// traffic.
    network: std::sync::Arc<tau_provider::OutboundNetworkPolicy>,
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
    /// Create an empty Codex runtime using one immutable startup network
    /// policy.
    #[must_use]
    pub fn new(network: std::sync::Arc<tau_provider::OutboundNetworkPolicy>) -> Self {
        Self {
            ws_pool: responses::pool::SharedWsPool::new(std::sync::Arc::clone(&network)),
            network,
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
        std::sync::Arc::clone(&self.network)
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
    fn stream(
        &self,
        agent_prompt_id: &str,
        config: &responses::ResponsesConfig,
        request: &Prompt<'_>,
        abort: &mut impl TurnAbort,
        on_update: &mut impl FnMut(StreamUpdate<'_>),
    ) -> Result<StreamDispatchResult, common::LlmError> {
        let ws_pool_before = self.ws_pool.stats();
        let session_id = request.session_id.as_str();
        let state = match responses::pool::run_turn_through_shared_pool(
            &self.ws_pool,
            config,
            agent_prompt_id,
            request,
            abort,
            on_update,
        ) {
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
        let mut progress = SemanticProgress::None;
        let result = self.stream(
            agent_prompt_id,
            config.wire(),
            request,
            abort,
            &mut |update| {
                if let StreamUpdate::Response(state) = update
                    && state.has_semantic_progress()
                {
                    progress = SemanticProgress::Parsed;
                }
                on_update(update);
            },
        );
        if let Ok(result) = &result
            && result.state.has_semantic_progress()
        {
            progress = SemanticProgress::Parsed;
        }
        if abort.is_aborted() {
            return AttemptOutcome::Canceled { progress };
        }
        match result {
            Ok(result) => AttemptOutcome::Finished(Box::new(result)),
            Err(common::LlmError::Canceled) => AttemptOutcome::Canceled { progress },
            // Preserve behavior at this site.
            // ast-grep-ignore: match-option-verbose
            Err(error) => match error.retry_decision() {
                Some(decision) => AttemptOutcome::Retry { decision, progress },
                None => AttemptOutcome::Terminal {
                    error: CodexError(error),
                    progress,
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
            // Preserve behavior at this site.
            // ast-grep-ignore: match-option-verbose
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
            .map_err(responses::pool::WsTurnError::into_llm_error)
            .map_err(CodexError)
    }

    /// Invalidates only sockets and in-flight publications owned by one profile
    /// namespace, preserving unrelated configured providers.
    pub fn invalidate_profile_websockets(&self, provider: &ProviderName) -> Result<(), CodexError> {
        self.ws_pool
            .invalidate_profile(provider.as_str())
            .map_err(responses::pool::WsTurnError::into_llm_error)
            .map_err(CodexError)
    }

    /// Invalidates the matching WebSocket chain before dispatch, then runs one
    /// joined unary remote compaction operation.
    pub fn compact(
        &self,
        agent_prompt_id: &str,
        config: &ResolvedConfig,
        request: &Prompt<'_>,
        abort: &mut impl TurnAbort,
    ) -> CompactOutcome {
        if let Err(error) = self.ws_pool.invalidate(config.wire(), request) {
            return if abort.is_aborted() {
                CompactOutcome::Canceled
            } else {
                CompactOutcome::Terminal(CodexError(error.into_llm_error()))
            };
        }
        if abort.is_aborted() {
            return CompactOutcome::Canceled;
        }
        let compact_result = responses::responses_compact(
            agent_prompt_id,
            config.wire(),
            request,
            abort,
            std::sync::Arc::clone(&self.network),
        );
        if abort.is_aborted() {
            return CompactOutcome::Canceled;
        }
        let output = match compact_result {
            Ok(output) => output,
            Err(common::LlmError::Canceled) => return CompactOutcome::Canceled,
            Err(error) => {
                // Preserve this behavior; the structural alternative is not semantics-neutral
                // here. ast-grep-ignore: match-option-verbose
                return match error.retry_decision() {
                    Some(decision) => CompactOutcome::Retry(decision),
                    None => CompactOutcome::Terminal(CodexError(error)),
                };
            }
        };
        if abort.is_aborted() {
            CompactOutcome::Canceled
        } else {
            CompactOutcome::Finished(output)
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
                tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseResponse
            }
            ProviderBackendTransport::Websocket => {
                tau_provider::debug_capture_writer::ProviderDebugCaptureClass::WebsocketResponse
            }
        })
        .unwrap_or(tau_provider::debug_capture_writer::ProviderDebugCaptureClass::UnknownResponse);
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
            tau_provider::debug_capture_writer::ProviderDebugCapture::new(
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
        responses::pool::WsTurnError::Canceled => false,
        responses::pool::WsTurnError::Other(error) => is_ws_capability_or_limit_llm_error(error),
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
            profile_namespace: provider.as_str().to_owned(),
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
        context_window: effective_context_window_for_model(model),
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
        standalone_compaction_threshold: is_gpt_5_6(model)
            .then_some((raw_context_window_for_model(model) * 9 / 10).max(1000)),
        est_uncached_input_cost_1m_usd: Some(prices.uncached_input),
        est_cached_input_cost_1m_usd: Some(prices.cached_input),
        est_output_cost_1m_usd: Some(prices.output),
    }
}

/// Basic standard-processing API prices from OpenAI's provider-owned table:
/// <https://developers.openai.com/api/docs/pricing>.
///
/// Tau deliberately ignores tiers, cache writes, batch discounts, and service
/// variants. These values are estimates for equivalent API use even when the
/// actual ChatGPT route is subscription-backed.
fn estimated_api_prices(model: &str) -> tau_proto::EstimatedApiCostRates {
    use tau_proto::{EstimatedApiCostRates, EstimatedUsdPerMillion as Price};

    let (uncached, cached, output) = match model {
        "gpt-5.6-sol" | "gpt-5.5" => (5_000_000, 500_000, 30_000_000),
        "gpt-5.6-terra" | "gpt-5.4" => (2_500_000, 250_000, 15_000_000),
        "gpt-5.6-luna" => (1_000_000, 100_000, 6_000_000),
        "gpt-5.4-mini" => (750_000, 75_000, 4_500_000),
        "gpt-5.3-codex" => (1_750_000, 175_000, 14_000_000),
        _ => return tau_proto::ESTIMATED_API_COST_FALLBACK,
    };
    EstimatedApiCostRates {
        uncached_input: Price::from_micro_usd(uncached),
        cached_input: Price::from_micro_usd(cached),
        output: Price::from_micro_usd(output),
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

fn raw_context_window_for_model(model: &str) -> u64 {
    if is_gpt_5_6(model) {
        GPT_5_6_RAW_CONTEXT_WINDOW
    } else {
        DEFAULT_RAW_CONTEXT_WINDOW
    }
}

fn effective_context_window_for_model(model: &str) -> u64 {
    raw_context_window_for_model(model) * EFFECTIVE_CONTEXT_WINDOW_PERCENT / 100
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
