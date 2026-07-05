//! ChatGPT/Codex provider backend helpers.
//!
//! This crate owns the ChatGPT/Codex model metadata and OpenAI Responses API
//! implementation, including HTTP/SSE, WebSocket transport, and pooled WS
//! sessions.

use std::collections::HashSet;
use std::sync::Mutex;

use tau_proto::{
    Effort, ModelId, ModelName, ModelTag, ProviderBackendTransport, ProviderModelInfo,
    ProviderName, ThinkingSummary, Verbosity,
};

pub const LOG_TARGET: &str = "provider-chatgpt";

/// ChatGPT/Codex backend base URL, without the final Responses path.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

const CONTEXT_WINDOW: u64 = 258400;
const CHATGPT_MODELS: &[&str] = &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex"];
const WS_RETRY_BUDGET_BEFORE_HTTP_FALLBACK: usize = 2;

pub mod common;
pub mod responses;

/// Prompt-turn cancellation source used by the WebSocket transport.
///
/// The synchronous provider loop needs a cancellation event that can wake a
/// blocking WebSocket receive. Implementors should register the supplied waker
/// with their native cancellation primitive and call it when the current turn
/// is canceled or the provider is shutting down.
pub trait TurnAbort {
    /// Return whether the current turn has already been canceled.
    fn is_aborted(&mut self) -> bool;

    /// Register a waker for future cancellation notifications.
    ///
    /// The returned guard must unregister the waker on drop so completed turns
    /// do not leave stale callbacks behind.
    fn register_waker(
        &mut self,
        waker: std::sync::Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker>;
}

/// Guard for a registered [`TurnAbort`] waker.
pub trait TurnAbortWaker {}

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

/// Runtime state for ChatGPT/Codex transports.
///
/// This owns the WebSocket pool and per-session WS fallback state so callers do
/// not need to know whether a prompt used WS or HTTP/SSE until the turn returns
/// its backend metadata.
pub struct ChatGptRuntime {
    ws_pool: responses::pool::SharedWsPool,
    ws_disabled: Mutex<HashSet<String>>,
}

/// Result of one ChatGPT/Codex streaming dispatch.
pub struct StreamDispatchResult {
    /// Fully accumulated provider stream state.
    pub state: common::StreamState,
    /// Transport that successfully served the turn.
    pub transport: ProviderBackendTransport,
    /// WebSocket pool counters changed by this turn, when available.
    pub ws_pool_delta: Option<tau_proto::WsPoolDelta>,
}

impl ChatGptRuntime {
    /// Create an empty ChatGPT runtime with no pooled WebSocket connections.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ws_pool: responses::pool::SharedWsPool::new(),
            ws_disabled: Mutex::new(HashSet::new()),
        }
    }

    /// Stream one prompt through the best available ChatGPT transport.
    ///
    /// WebSocket is tried first when supported. Known WS-capability or limit
    /// failures disable WS for this session and transparently fall back to
    /// HTTP/SSE; retryable WS failures are surfaced until this turn's internal
    /// retry budget is exhausted, then also fall back to HTTP/SSE for this
    /// session. The `abort` source is checked before starting WS work and is
    /// also registered as a wake source while a WS turn is blocked on
    /// provider events; canceled turns return `LlmError::HttpStatus(499,
    /// "cancelled by harness")`.
    pub fn stream(
        &self,
        agent_prompt_id: &str,
        config: &responses::ResponsesConfig,
        request: &common::PromptPayload<'_>,
        turn_state: &mut ChatGptTurnState,
        abort: &mut impl TurnAbort,
        on_update: &mut impl FnMut(&common::StreamState),
    ) -> Result<StreamDispatchResult, common::LlmError> {
        let ws_pool_before = self.ws_pool.stats();
        let mut transport = ProviderBackendTransport::HttpSse;
        let session_id = request.session_id.as_str();
        let try_ws = config.supports_websocket
            && self
                .ws_disabled
                .lock()
                .map(|disabled| !disabled.contains(session_id))
                .unwrap_or(false);
        let state = if try_ws {
            match responses::pool::run_turn_through_shared_pool(
                &self.ws_pool,
                config,
                agent_prompt_id,
                request,
                abort,
                on_update,
            ) {
                Ok(state) => {
                    turn_state.ws_failures = 0;
                    transport = ProviderBackendTransport::Websocket;
                    state
                }
                Err(error) if should_disable_ws_error(&error) => {
                    let error = error.into_llm_error();
                    tracing::warn!(
                        target: LOG_TARGET,
                        session_id,
                        "WS path failed ({error}); falling back to HTTP for this session",
                    );
                    if let Ok(mut disabled) = self.ws_disabled.lock() {
                        disabled.insert(session_id.to_owned());
                    }
                    responses::responses_stream(agent_prompt_id, config, request, on_update)?
                }
                Err(other) => {
                    let error = other.into_llm_error();
                    if error.retry_after().is_some() {
                        turn_state.ws_failures += 1;
                        if turn_state.ws_failures <= turn_state.ws_retry_budget {
                            tracing::warn!(
                                target: LOG_TARGET,
                                session_id,
                                ws_retry_failures = turn_state.ws_failures,
                                ws_retry_budget = turn_state.ws_retry_budget,
                                "WS path failed with retryable error ({error}); retrying WS before HTTP fallback",
                            );
                            return Err(error);
                        }
                        tracing::warn!(
                            target: LOG_TARGET,
                            session_id,
                            ws_retry_failures = turn_state.ws_failures,
                            ws_retry_budget = turn_state.ws_retry_budget,
                            "WS retry budget exhausted ({error}); falling back to HTTP for this session",
                        );
                        if let Ok(mut disabled) = self.ws_disabled.lock() {
                            disabled.insert(session_id.to_owned());
                        }
                        transport = ProviderBackendTransport::HttpSse;
                        responses::responses_stream(agent_prompt_id, config, request, on_update)?
                    } else {
                        return Err(error);
                    }
                }
            }
        } else {
            responses::responses_stream(agent_prompt_id, config, request, on_update)?
        };
        let ws_pool_delta = ws_pool_before.and_then(|before| {
            self.ws_pool
                .stats()
                .map(|after| compute_ws_pool_delta(before, after))
        });
        Ok(StreamDispatchResult {
            state,
            transport,
            ws_pool_delta,
        })
    }

    /// Best-effort non-generating prewarm for a later ChatGPT prompt.
    pub fn prewarm(
        &self,
        config: &responses::ResponsesConfig,
        session_id: &str,
        request: &common::PromptPayload<'_>,
    ) -> Result<(), common::LlmError> {
        let ws_disabled_for_session = self
            .ws_disabled
            .lock()
            .map(|disabled| disabled.contains(session_id))
            .unwrap_or(true);
        if !config.supports_websocket || ws_disabled_for_session {
            tracing::debug!(
                target: LOG_TARGET,
                session_id,
                "skipping prompt prewarm: websocket prewarm unsupported",
            );
            return Ok(());
        }

        match responses::pool::run_prewarm_through_shared_pool(
            &self.ws_pool,
            config,
            session_id,
            request,
        ) {
            Ok(_) => Ok(()),
            Err(error) if should_disable_ws(&error) => {
                if let Ok(mut disabled) = self.ws_disabled.lock() {
                    disabled.insert(session_id.to_owned());
                }
                Err(error)
            }
            Err(error) => Err(error),
        }
    }
}

impl Default for ChatGptRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-turn state for ChatGPT/Codex transport fallback.
pub struct ChatGptTurnState {
    ws_failures: usize,
    ws_retry_budget: usize,
}

impl ChatGptTurnState {
    /// Create state for one prompt turn from the outer provider retry budget.
    ///
    /// ChatGPT may spend a small prefix of those retries on WebSocket before
    /// falling back to HTTP/SSE for the session.
    #[must_use]
    pub fn new(max_provider_retries: usize) -> Self {
        Self {
            ws_failures: 0,
            ws_retry_budget: max_provider_retries.min(WS_RETRY_BUDGET_BEFORE_HTTP_FALLBACK),
        }
    }
}

fn should_disable_ws_error(error: &responses::pool::WsTurnError) -> bool {
    match error {
        responses::pool::WsTurnError::Canceled => false,
        responses::pool::WsTurnError::Other(error) => should_disable_ws(error),
    }
}

fn should_disable_ws(error: &common::LlmError) -> bool {
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
    CHATGPT_MODELS
        .iter()
        .map(|model| model_info(provider, model))
        .collect()
}

/// Returns a Responses backend config for one ChatGPT/Codex model.
#[must_use]
pub fn config_for_model(
    model: &ModelName,
    access_token: String,
    account_id: Option<String>,
) -> responses::ResponsesConfig {
    let model_id = model.as_str();
    responses::ResponsesConfig {
        surface: responses::ResponsesSurface::ChatGpt,
        base_url: DEFAULT_BASE_URL.to_owned(),
        api_key: access_token,
        model_id: model_id.to_owned(),
        context_window: CONTEXT_WINDOW,
        account_id,
        supports_reasoning_effort: true,
        supports_reasoning_summary: true,
        supports_verbosity: model_id.starts_with("gpt-5"),
        supports_phase: is_known_phase_capable_model_id(model_id),
        supports_encrypted_reasoning: true,
        supports_websocket: true,
        supports_compaction: true,
        supports_prompt_cache_key: true,
    }
}

fn model_info(provider: &ProviderName, model: &str) -> ProviderModelInfo {
    ProviderModelInfo {
        id: ModelId::new(provider.clone(), ModelName::new(model)),
        display_name: None,
        tags: vec![
            ModelTag::new("shell:chatgpt"),
            ModelTag::new("tools:custom-text"),
        ],
        default_affinity: default_affinity_for_model(model),
        context_window: CONTEXT_WINDOW,
        efforts: efforts_for_model(model),
        verbosities: verbosities_for_model(model),
        thinking_summaries: vec![
            ThinkingSummary::Off,
            ThinkingSummary::Auto,
            ThinkingSummary::Concise,
            ThinkingSummary::Detailed,
        ],
        supports_compaction: true,
    }
}

fn default_affinity_for_model(model: &str) -> i32 {
    match model {
        "gpt-5.5" => 400,
        "gpt-5.4" => 300,
        "gpt-5.3-codex" => 200,
        "gpt-5.4-mini" => 100,
        _ => 0,
    }
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
    efforts
}

fn supports_xhigh(model: &str) -> bool {
    if model.contains("mini") || model.contains("nano") {
        return false;
    }
    [
        "gpt-5.5",
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
