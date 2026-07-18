//! ChatGPT/Codex provider backend helpers.
//!
//! This crate owns the ChatGPT/Codex model metadata and OpenAI Responses API
//! implementation, including pooled WebSocket inference and supported unary
//! HTTPS control-plane operations.
//! Component boundaries and provider-visible replay are summarized in
//! `ARCH-tau-provider-codex` and
//! `SPEC-tau-provider-codex-streaming-replay`.
//! Its transport test boundary is recorded in
//! `DECISION-tau-provider-codex-backend-testing-boundary`.

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

pub mod common;
pub mod oauth;
pub mod quota;
pub mod responses;

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
///
/// The cooperative wake contract is recorded in
/// `DECISION-tau-provider-codex-cooperative-cancellation`.
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
    pub state: common::StreamState,
    /// WebSocket transport fact retained for the stable protocol descriptor.
    pub transport: ProviderBackendTransport,
    /// WebSocket pool counters changed by this turn, when available.
    pub ws_pool_delta: Option<tau_proto::WsPoolDelta>,
}

/// Semantically disjoint live updates from one ChatGPT dispatch.
pub enum StreamUpdate<'a> {
    /// A fresh WebSocket upgrade is about to start. This is WebSocket-only and
    /// may occur more than once when a logical turn replaces a failed socket.
    Connecting,
    /// A response-state observation for progress sampling. It may repeat
    /// unchanged during quiet waits.
    Response(&'a common::StreamState),
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
    pub fn stream(
        &self,
        agent_prompt_id: &str,
        config: &responses::ResponsesConfig,
        request: &common::PromptPayload<'_>,
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
                    "WS path failed with capability/limit error; surfacing error without HTTP fallback: {error}",
                );
                return Err(error);
            }
            Err(other) => {
                let error = other.into_llm_error();
                tracing::warn!(
                    target: LOG_TARGET,
                    session_id,
                    "WS path failed; surfacing error without HTTP fallback: {error}",
                );
                return Err(error);
            }
        };
        let ws_pool_delta = ws_pool_before.and_then(|before| {
            self.ws_pool
                .stats()
                .map(|after| compute_ws_pool_delta(before, after))
        });
        Ok(StreamDispatchResult {
            state,
            transport: ProviderBackendTransport::Websocket,
            ws_pool_delta,
        })
    }

    /// Best-effort non-generating prewarm for a later ChatGPT prompt.
    ///
    /// The caller must run this on supervised blocking work rather than its
    /// event loop. `abort` owns cancellation of connection, response wait, and
    /// socket installation. A fresh upgrade is bounded to 30 seconds and the
    /// response has a separate 30-second absolute bound. Success retains the
    /// socket for the matching real prompt; failure leaves no reservation or
    /// cached replacement behind. Socket publication unregisters its abort
    /// callback and then rechecks [`TurnAbort::is_aborted`] before releasing
    /// the same-key reservation, so custom abort implementations cannot
    /// publish cancellation that was already authoritative at callback
    /// retirement.
    pub fn prewarm(
        &self,
        config: &responses::ResponsesConfig,
        session_id: &str,
        request: &common::PromptPayload<'_>,
        abort: &mut impl TurnAbort,
    ) -> Result<(), common::LlmError> {
        responses::pool::run_prewarm_through_shared_pool(
            &self.ws_pool,
            config,
            session_id,
            request,
            abort,
        )
        .map(|_| ())
    }

    /// Invalidates cached sockets and prevents currently reserved sockets from
    /// being reinstalled after a provider profile or session boundary.
    pub fn invalidate_all_websockets(&self) -> Result<(), common::LlmError> {
        self.ws_pool
            .invalidate_all()
            .map_err(responses::pool::WsTurnError::into_llm_error)
    }

    /// Runs unary remote compaction and invalidates any pre-boundary WebSocket
    /// chain only after the replacement window is accepted from upstream.
    pub fn compact(
        &self,
        agent_prompt_id: &str,
        config: &responses::ResponsesConfig,
        request: &common::PromptPayload<'_>,
        abort: &mut impl TurnAbort,
    ) -> Result<Vec<tau_proto::ContextItem>, common::LlmError> {
        let output = responses::responses_compact(
            agent_prompt_id,
            config,
            request,
            abort,
            std::sync::Arc::clone(&self.network),
        )?;
        self.ws_pool
            .invalidate(config, request)
            .map_err(|error| error.into_llm_error())?;
        Ok(output)
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
    mode: responses::ResponsesMode,
) -> Vec<ProviderModelInfo> {
    CHATGPT_MODELS
        .iter()
        .map(|model| model_info(provider, model, effective_mode(model, mode)))
        .collect()
}

/// Returns a Responses backend config for one ChatGPT/Codex model.
#[must_use]
pub fn config_for_model(
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
pub fn config_for_model_mode(
    model: &ModelName,
    access_token: String,
    account_id: Option<String>,
    requested_mode: responses::ResponsesMode,
) -> responses::ResponsesConfig {
    let model_id = model.as_str();
    let mode = effective_mode(model_id, requested_mode);
    responses::ResponsesConfig {
        mode,
        base_url: DEFAULT_BASE_URL.to_owned(),
        api_key: access_token,
        model_id: model_id.to_owned(),
        raw_context_window: raw_context_window_for_model(model_id),
        account_id,
        supports_reasoning_effort: true,
        supports_reasoning_summary: true,
        supports_verbosity: model_id.starts_with("gpt-5"),
        supports_phase: is_known_phase_capable_model_id(model_id),
        supports_encrypted_reasoning: true,
        supports_compaction: !is_gpt_5_6(model_id),
        supports_prompt_cache_key: true,
    }
}

fn model_info(
    provider: &ProviderName,
    model: &str,
    mode: responses::ResponsesMode,
) -> ProviderModelInfo {
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

    n >= 4 || (n == 3 && suffix.starts_with("codex"))
}

#[cfg(test)]
mod tests;
