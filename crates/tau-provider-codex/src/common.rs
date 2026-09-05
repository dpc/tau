//! Types shared by the ChatGPT/Codex Responses transports.

#[cfg(test)]
use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use base64::engine as path_base64_engine;
use tau_proto::{
    CborValue, ContentPart, ContextItem, ContextRole, MessageItem, OpaqueProviderItem,
    PromptContext, PromptOriginator, ProviderFailureKind, ProviderResponseCompactionStatus,
    ProviderResponseCompactionUpdate, ProviderResponseTextDelta, ProviderTokenUsage,
    ReasoningTextItem, ReasoningTextKind, ResponsesToolCallEnvelope, SessionId, ToolCallItem,
    ToolDefinition,
};
use tau_provider::retry_policy::{RetryClass, RetryDecision, classify_error_code};
use tau_provider::{StreamRepetitionGuard, StreamRepetitionKey};
use uuid::Uuid;

use crate::attempt_failure as path_crate_attempt_failure;
use crate::canonical_identifier::CanonicalIdentifierFamily;

/// The parts of a prompt needed by an LLM backend client.
pub struct PromptPayload<'a> {
    /// Provider instructions for this prompt.
    pub system_prompt: &'a str,
    /// Ordered semantic transcript context.
    pub context: &'a PromptContext,
    /// Effective client-executed tool definitions.
    pub tools: &'a [ToolDefinition],
    /// Provider-hosted tools selected for this materialized prompt.
    pub hosted_tools: &'a [tau_proto::HostedToolDefinition],
    /// Per-prompt model knobs (effort / verbosity / thinking-summary).
    /// Each field is honored only when the backend's config reports
    /// support for the corresponding provider feature.
    pub params: tau_proto::ModelParams,
    /// Whether the model may emit tool-call output on this turn.
    /// `Auto` (default) lets the model decide; `None` forces a text
    /// answer. Tools and `system_prompt` are still passed verbatim
    /// in either case so the prompt-cache prefix stays stable.
    pub tool_choice: tau_proto::ToolChoice,
    /// Server-side context-management compaction metadata, when enabled for
    /// this prompt/model.
    pub compaction: Option<tau_proto::PromptCompactionContext>,

    /// Who originated this prompt — the interactive user, or a side query
    /// such as a harness-owned delegated sub-agent. This remains available for
    /// provenance, retry policy, and provider lifecycle events, but it must not
    /// affect prompt-cache routing.
    pub originator: &'a PromptOriginator,
    /// Legacy request flag for callers that once requested the user's cache
    /// bucket explicitly. Prompt-cache routing is now stable per agent, so this
    /// no longer changes the wire `prompt_cache_key`.
    pub share_user_cache_key: bool,
    /// Harness session this prompt belongs to. Used for debug paths and
    /// tracing; the Responses WebSocket pool keys upstream sockets by the
    /// prompt-cache UUID instead.
    /// Backends without session-scoped diagnostics ignore this.
    pub session_id: &'a SessionId,
    /// Durable agent this prompt belongs to.
    pub agent_id: &'a tau_proto::AgentId,
    /// Whether provider debug request/response captures may be written for this
    /// prompt's current session.
    ///
    /// The harness must supply this from explicit session persistence state.
    /// Backends must not infer durability from filesystem paths because an
    /// ephemeral session can reuse a session id that already has durable state
    /// from an earlier run.
    pub debug_provider_requests: bool,
}

impl PromptPayload<'_> {
    /// Derive the OpenAI-style prompt-cache UUID for this prompt and protocol
    /// identity on `base_url`.
    ///
    /// ChatGPT WebSocket upgrades use the same UUID for their upstream
    /// `session-id` and `thread-id` headers, so callers should derive the value
    /// through this method rather than duplicating the hashing inputs.
    #[must_use]
    pub fn prompt_cache_key(&self, base_url: &str, mode: crate::CodexMode) -> String {
        prompt_cache_key_for(base_url, self.agent_id, mode)
    }
}

/// Transport / protocol error returned from any LLM backend stream.
#[derive(Debug)]
pub enum LlmError {
    /// Error paired with bounded evidence captured at its observation boundary.
    Observed {
        /// Existing typed provider error used for retry and terminal policy.
        source: Box<LlmError>,
        /// Opaque bounded parser/transport observation used by diagnostics.
        evidence: Box<crate::attempt_failure::AttemptFailureEvidence>,
    },
    /// Shared outbound policy or route failure with a redacted projection.
    Outbound(tau_provider::OutboundError),
    HttpStatus(u16, String),
    /// HTTP status preserving a trusted `Retry-After` header.
    HttpStatusRetryAfter(u16, String, Duration),
    /// HTTP status preserving a trusted transport-level retry hint.
    /// WebSocket error event preserving its canonical structured code
    /// separately from untrusted display prose.
    StreamError {
        /// Bounded display detail.
        body: String,
        /// Canonical top-level or error-envelope code/type.
        code: Option<String>,
        /// Trusted reset hint parsed from structured response fields.
        retry_after: Option<Duration>,
    },
    /// WebSocket close metadata retained without exposing raw reason text to
    /// captures.
    WsClosed(crate::attempt_failure::WsTermination),
    /// Prompt cancellation observed from Tau's trusted local abort source.
    Canceled,
    /// Mutable URL, credential, or account configuration could not build a
    /// request; see `SPEC-tau-provider-codex-retry-classification`.
    ReloadableConfig(String),
    /// Provider response was syntactically readable but unsafe to accept.
    InvalidResponse(String),
    #[allow(dead_code)]
    Io(std::io::Error),
    Json(serde_json::Error),
    Vcr(tau_vcr::VcrError),
    RepetitionDetected(tau_provider::StreamRepetition),
    /// The provider route rejected Tau's required WebSocket transport.
    WsUpgradeRequired,
    /// A canonical provider envelope proved that replaying the request is
    /// futile.
    ProviderFailure(ProviderFailureKind, String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Observed { source, .. } => source.fmt(f),
            Self::Outbound(error) => error.fmt(f),
            Self::HttpStatus(code, body) => write!(f, "HTTP {code}: {body}"),
            Self::HttpStatusRetryAfter(code, body, _) => write!(f, "HTTP {code}: {body}"),
            Self::StreamError { body, .. } => write!(f, "HTTP 0: {body}"),
            Self::WsClosed(termination) => write!(f, "WebSocket closed ({termination:?})"),
            Self::Canceled => write!(f, "cancelled by harness"),
            Self::ReloadableConfig(error) => write!(f, "local request construction: {error}"),
            Self::InvalidResponse(error) => write!(f, "invalid provider response: {error}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::Vcr(e) => write!(f, "VCR error: {e}"),
            Self::RepetitionDetected(repetition) => write!(f, "{repetition}"),
            Self::WsUpgradeRequired => {
                f.write_str("Codex requires WebSocket; Tau has no HTTP/SSE fallback")
            }
            Self::ProviderFailure(_, detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for LlmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Observed { source, .. } => Some(source),
            Self::Outbound(error) => Some(error),
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::Vcr(e) => Some(e),
            Self::RepetitionDetected(_)
            | Self::WsUpgradeRequired
            | Self::ProviderFailure(_, _)
            | Self::Canceled
            | Self::InvalidResponse(_)
            | Self::ReloadableConfig(_)
            | Self::HttpStatus(_, _)
            | Self::HttpStatusRetryAfter(_, _, _)
            | Self::StreamError { .. }
            | Self::WsClosed(_) => None,
        }
    }
}

impl LlmError {
    /// Returns whether the canonical HTTP status rejected the presented bearer.
    #[must_use]
    pub(crate) fn is_canonical_unauthorized(&self) -> bool {
        matches!(
            self.root_error(),
            Self::HttpStatus(401, _) | Self::HttpStatusRetryAfter(401, _, _)
        )
    }

    /// Returns whether canonical provider code proves v2 compaction
    /// unsupported.
    #[must_use]
    pub(crate) fn is_compaction_route_unavailable(&self) -> bool {
        matches!(
            self.stream_error_code(),
            Some("compaction_not_supported" | "unsupported_compaction")
        )
    }

    /// Attach opaque evidence without changing the underlying policy error.
    pub(crate) fn observed(self, evidence: crate::attempt_failure::AttemptFailureEvidence) -> Self {
        Self::Observed {
            source: Box::new(self),
            evidence: Box::new(evidence),
        }
    }

    #[must_use]
    /// Borrow the nearest parser/transport evidence, when one was observed.
    pub(crate) fn evidence(&self) -> Option<&crate::attempt_failure::AttemptFailureEvidence> {
        match self {
            Self::Observed { evidence, .. } => Some(evidence),
            _ => None,
        }
    }

    #[must_use]
    /// Borrow the underlying policy error for compatibility classification.
    pub(crate) fn root_error(&self) -> &Self {
        match self {
            Self::Observed { source, .. } => source.root_error(),
            _ => self,
        }
    }

    /// Classify whether and how the logical prompt should be retried.
    ///
    /// Unknown remote failures deliberately retry. Only errors with positive
    /// evidence that repeating the unchanged operation is futile are terminal.
    #[must_use]
    pub fn retry_decision(&self) -> Option<RetryDecision> {
        match self {
            Self::Observed { source, .. } => source.retry_decision(),
            Self::Outbound(error) => Some(RetryDecision::new(outbound_retry_class(error.kind()))),
            Self::Io(_) => Some(RetryDecision::new(RetryClass::Transport)),
            Self::WsClosed(_) => Some(RetryDecision::new(RetryClass::Transport)),
            Self::Json(_) => Some(RetryDecision::new(RetryClass::Unknown)),
            Self::Vcr(_)
            | Self::RepetitionDetected(_)
            | Self::WsUpgradeRequired
            | Self::ProviderFailure(_, _)
            | Self::Canceled => None,
            Self::ReloadableConfig(_) => Some(RetryDecision::new(RetryClass::Auth)),
            Self::InvalidResponse(_) => None,
            Self::HttpStatus(code, body) => classify_http_status(*code, body, None),
            Self::HttpStatusRetryAfter(code, body, hint) => {
                classify_http_status(*code, body, Some(*hint))
            }
            Self::StreamError {
                body,
                code,
                retry_after,
            } => classify_provider_error(0, body, code.as_deref(), *retry_after),
        }
    }

    /// Whether this error is plausibly transient and worth retrying.
    ///
    /// We treat transport hiccups, mid-stream IO breaks, and
    /// server-side stream errors (overload, upstream timeout) as
    /// retryable. JSON parse failures, missing-choices, and 4xx
    /// statuses other than 408/425/429 are treated as our bug or a
    /// deterministic request-level rejection — retrying just burns
    /// quota.
    #[cfg(test)]
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_decision()
            .map(|decision| decision.retry_after.unwrap_or(Duration::ZERO))
    }

    /// Return the typed terminal provider category, when one was proven.
    #[must_use]
    pub fn failure_kind(&self) -> Option<ProviderFailureKind> {
        match self {
            Self::Observed { source, .. } => source.failure_kind(),
            Self::WsUpgradeRequired => Some(ProviderFailureKind::RequestRejected),
            Self::ProviderFailure(kind, _) => Some(*kind),
            Self::HttpStatus(status, body) | Self::HttpStatusRetryAfter(status, body, _) => {
                http_failure_kind(*status, body)
            }
            Self::StreamError { code, .. }
                if code.as_deref() == Some("context_length_exceeded") =>
            {
                Some(ProviderFailureKind::ContextWindowExceeded)
            }
            _ => None,
        }
    }
}

impl LlmError {
    /// Returns the canonical structured WebSocket error code, when present.
    #[must_use]
    pub(crate) fn stream_error_code(&self) -> Option<&str> {
        match self {
            Self::Observed { source, .. } => source.stream_error_code(),
            Self::StreamError { code, .. } => code.as_deref(),
            _ => None,
        }
    }
}

fn outbound_retry_class(kind: tau_provider::OutboundErrorKind) -> RetryClass {
    match kind {
        tau_provider::OutboundErrorKind::InvalidConfiguration
        | tau_provider::OutboundErrorKind::ProxyAuthentication => RetryClass::Auth,
        tau_provider::OutboundErrorKind::Transport
        | tau_provider::OutboundErrorKind::Deadline
        | tau_provider::OutboundErrorKind::Protocol => RetryClass::Transport,
    }
}

fn classify_http_status(
    code: u16,
    body: &str,
    transport_hint: Option<Duration>,
) -> Option<RetryDecision> {
    // Keep adapter classification independent from UI prose.
    let provider_code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            CanonicalIdentifierFamily::from_value(&value)
                .classified()
                .map(ToOwned::to_owned)
        });
    if provider_code.as_deref() == Some("context_length_exceeded") {
        return None;
    }
    if code == 401 {
        return Some(RetryDecision::new(RetryClass::Auth).with_retry_after(transport_hint));
    }
    if http_failure_kind(code, body).is_some() {
        return None;
    }
    classify_provider_error(code, body, provider_code.as_deref(), transport_hint)
}

fn classify_provider_error(
    code: u16,
    body: &str,
    provider_code: Option<&str>,
    transport_hint: Option<Duration>,
) -> Option<RetryDecision> {
    let class = provider_code
        .map(classify_error_code)
        .filter(|class| *class != RetryClass::Unknown)
        .unwrap_or_else(|| match code {
            408 | 409 | 425 => RetryClass::Transport,
            429 => RetryClass::Throttle,
            500..=599 => RetryClass::Overload,
            401 | 403 => RetryClass::Auth,
            0 if is_provider_stream_idle_timeout_body(body) => RetryClass::Transport,
            _ => RetryClass::Unknown,
        });
    let body_hint = canonical_json_reset_hint(body, SystemTime::now());
    Some(
        RetryDecision::new(class)
            .with_retry_after(transport_hint.into_iter().chain(body_hint).max()),
    )
}

fn canonical_json_reset_hint(body: &str, now: SystemTime) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    canonical_reset_objects(&value)
        .into_iter()
        .flatten()
        .find_map(|object| reset_hint_from_object(object, now))
}

fn canonical_reset_objects(value: &serde_json::Value) -> [Option<&serde_json::Value>; 3] {
    [
        Some(value),
        value.get("error"),
        value
            .get("response")
            .and_then(|response| response.get("error")),
    ]
}

fn reset_hint_from_object(value: &serde_json::Value, now: SystemTime) -> Option<Duration> {
    let object = value.as_object()?;
    if let Some(seconds) = object
        .get("resets_in_seconds")
        .and_then(serde_json::Value::as_u64)
    {
        return Some(Duration::from_secs(seconds));
    }
    let reset_at = object.get("resets_at")?.as_u64()?;
    let now = now.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(reset_at.saturating_sub(now)))
}

fn http_failure_kind(status: u16, body: &str) -> Option<ProviderFailureKind> {
    let identifiers = canonical_error_identifiers(body);
    if identifiers
        .iter()
        .any(|code| code == "context_length_exceeded")
    {
        return Some(ProviderFailureKind::ContextWindowExceeded);
    }
    let known_transient = identifiers
        .iter()
        .any(|code| classify_error_code(code) != RetryClass::Unknown);
    if !known_transient && matches!(status, 400 | 404 | 413 | 422) {
        Some(ProviderFailureKind::RequestRejected)
    } else {
        None
    }
}

fn canonical_error_identifiers(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    CanonicalIdentifierFamily::from_value(&value)
        .iter()
        .map(ToOwned::to_owned)
        .collect()
}

/// Account-level limits that won't clear with any reasonable backoff —
/// matched against the `(type=…)` suffix that
/// `responses::apply_event` attaches to a `stream error: …` body. New
/// upstream error types can be added here as we encounter them;
/// false negatives just mean we burn a few retries before failing.
///
/// Exposed for the WS pool's `is_recoverable_ws_error` carve-out so
/// the pool doesn't reopen a socket just to hit the same cap on the
/// fresh one.
pub fn is_account_limit_body(body: &str) -> bool {
    body.contains("(type=usage_limit_reached)")
        || body.contains("(type=rate_limit_exceeded)")
        || body.contains("(type=quota_exceeded)")
        || body.contains("(type=billing_hard_limit_reached)")
        || body.contains("(type=insufficient_quota)")
}

/// Provider stream idle watchdog failures are terminal for the current turn.
///
/// Retrying would keep a visibly stalled prompt in-flight for another full idle
/// window instead of promptly unblocking queued work with a terminal provider
/// error.
pub fn is_provider_stream_idle_timeout_body(body: &str) -> bool {
    body.contains("provider stream idle timeout")
}

/// One provider output item as it is incrementally assembled from a
/// streaming response. This is intentionally item-shaped: final
/// `ProviderResponseFinished.output_items` must be a projection of the
/// stream's item timeline, not a late re-bucketing of text/reasoning/tool
/// calls.
#[derive(Clone, Debug)]
pub enum OutputItemAccumulator {
    Empty,
    Message(MessageAccumulator),
    ToolCall(ToolCallAccumulator),
    Reasoning(OpaqueProviderItem),
    Compaction(Option<OpaqueProviderItem>),
    UnknownProviderItem(OpaqueProviderItem),
}

/// Accumulates one assistant message item across text deltas.
#[derive(Clone, Debug, Default)]
pub struct MessageAccumulator {
    /// Accumulated visible assistant text for this output item.
    pub text: String,
    /// Optional Responses assistant message phase captured for this item.
    pub phase: Option<tau_proto::MessagePhase>,
    /// Raw Responses assistant message item used for replay fidelity.
    pub responses_raw_json: Option<String>,
    /// Validated semantic citation metadata for the accumulated text.
    pub citations: Vec<ContentPart>,
}

/// Accumulated streaming state shared by both backends.
pub struct StreamState {
    /// Provider-indexed semantic accumulators.
    ///
    /// Production mutation must use this type's indexed helpers so the cached
    /// aggregate fields remain coherent.
    pub(crate) output_items: Vec<OutputItemAccumulator>,
    /// Output indices with a provider-hosted web search in progress. This is
    /// transient display state and never enters replay payload accounting.
    active_web_searches: std::collections::BTreeMap<usize, String>,
    /// Latest hosted web-search lifecycle transition and its revision.
    web_search_lifecycle: Option<(u64, String, bool)>,
    /// Monotonic revision for hosted web-search lifecycle transitions.
    web_search_lifecycle_revision: u64,
    /// Cumulative UTF-8 bytes across all assistant message slots.
    assistant_text_bytes: u64,
    /// Cumulative UTF-8 bytes across all non-visible tool input slots.
    non_visible_output_bytes: u64,
    /// Number of output slots containing replay-significant semantic output.
    semantic_output_items: usize,
    /// Number of output slots qualifying for first-semantic-output timing.
    timed_output_items: usize,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) cached_tokens: Option<u64>,
    /// Provider-reported cache-write input tokens.
    pub(crate) cache_write_tokens: Option<u64>,
    /// Exact response-local cache-read ceiling established by request lowering.
    pub(crate) prompt_cache_read_ceiling_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    /// Provider-supplied reasoning summary accumulated so far. `None`
    /// when the provider hasn't emitted any summary content (or when
    /// summaries weren't requested).
    pub(crate) thinking: Option<String>,
    /// Output item index the displayable reasoning summary belongs to.
    thinking_output_index: Option<usize>,
    /// Provider-supplied `response.id`, used by the harness to chain
    /// the next turn off this one via `previous_response_id`. Only
    /// populated by the Responses backend; the Chat Completions
    /// backend leaves this `None`.
    pub(crate) response_id: Option<String>,
    /// Raw terminal provider event for Responses streams (`response.completed`
    /// / `response.done`), retained for per-session debug captures. Other
    /// backends leave this empty.
    pub(crate) provider_terminal_event: Option<serde_json::Value>,
    /// Cumulative raw provider response bytes received by the transport for
    /// this prompt before semantic parsing. Used for live progress when the
    /// provider has delivered bytes that have not yet formed parseable output.
    transport_response_bytes: u64,
    /// A stale `previous_response_id` was rejected and this successful stream
    /// came from the full-replay retry.
    pub(crate) stale_chain_fallback: bool,
    /// Bounded exact repetition guard for this provider generation.
    repetition_guard: StreamRepetitionGuard,
    /// Latest supported WebSocket account-quota observation for this turn.
    pub(crate) quota_observation: Option<crate::quota::RollingQuotaObservation>,
    /// Parser evidence work permitted for this response.
    pub(crate) provider_evidence_mode: crate::attempt_failure::ProviderEvidenceMode,
    /// Exact logical retained-state bytes admitted by the live WebSocket owner.
    retained_state_bytes: u64,
    /// Assistant bytes copied by terminal aggregate fallback in test builds.
    #[cfg(test)]
    aggregate_assistant_text_copied_bytes: Cell<u64>,
}

/// Provider token counters accumulated by one completed response.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProviderTokenCounts {
    /// Total prompt/input tokens.
    pub input: Option<u64>,
    /// Prompt tokens served from provider cache.
    pub cached: Option<u64>,
    /// Generated output tokens.
    pub output: Option<u64>,
}

/// Tracks text already emitted to transient response update streams.
#[derive(Debug, Default)]
pub struct StreamDeltaEmitter {
    /// Last assistant message text emitted per provider output index.
    emitted_messages: BTreeMap<usize, String>,
    /// Last reasoning text emitted per provider output index.
    emitted_reasoning: BTreeMap<usize, String>,
}

impl StreamDeltaEmitter {
    /// Returns newly appended assistant/reasoning deltas since the last call.
    pub fn deltas(&mut self, state: &StreamState) -> Vec<ProviderResponseTextDelta> {
        let mut deltas = Vec::new();
        for (index, item) in state.output_items.iter().enumerate() {
            if let OutputItemAccumulator::Message(message) = item {
                self.push_message_delta(index, message, &mut deltas);
            }
        }
        if let Some(thinking) = state
            .thinking
            .as_deref()
            .filter(|thinking| !thinking.is_empty())
        {
            let index = state.thinking_output_index.unwrap_or(0);
            let previous = self.emitted_reasoning.entry(index).or_default();
            if let Some(delta) = append_suffix(previous, thinking) {
                deltas.push(ProviderResponseTextDelta::ReasoningText {
                    output_index: index as u32,
                    kind: ReasoningTextKind::Summary,
                    text: delta,
                });
            }
        }
        deltas
    }

    fn push_message_delta(
        &mut self,
        index: usize,
        message: &MessageAccumulator,
        deltas: &mut Vec<ProviderResponseTextDelta>,
    ) {
        if message.text.is_empty() {
            return;
        }
        let previous = self.emitted_messages.entry(index).or_default();
        if let Some(delta) = append_suffix(previous, &message.text) {
            deltas.push(ProviderResponseTextDelta::Message {
                output_index: index as u32,
                text: delta,
                phase: message.phase,
            });
        }
    }
}

fn append_suffix(previous: &mut String, current: &str) -> Option<String> {
    if current == previous {
        return None;
    }
    if let Some(suffix) = current.strip_prefix(previous.as_str()) {
        previous.push_str(suffix);
        if suffix.is_empty() {
            None
        } else {
            Some(suffix.to_owned())
        }
    } else {
        tracing::trace!("stream text stopped being append-only; waiting for final response");
        None
    }
}

/// Accumulates one tool call across streaming chunks.
#[derive(Clone, Debug)]
pub struct ToolCallAccumulator {
    pub id: String,
    pub name: String,
    pub tool_type: tau_proto::ToolType,
    pub arguments_json: String,
    pub responses_envelope: ResponsesToolCallEnvelope,
}

/// Mutable access to one tool-call slot that reconciles cached stream totals
/// when the caller finishes changing the accumulator.
pub(crate) struct ToolCallAccumulatorMut<'a> {
    /// Owning stream state.
    state: &'a mut StreamState,
    /// Provider output index of the borrowed tool call.
    output_index: usize,
    /// Slot metrics before mutable access began.
    old: SlotMetrics,
}

impl std::ops::Deref for ToolCallAccumulatorMut<'_> {
    type Target = ToolCallAccumulator;

    fn deref(&self) -> &Self::Target {
        let OutputItemAccumulator::ToolCall(call) = &self.state.output_items[self.output_index]
        else {
            unreachable!("tool-call guard owns a tool-call slot");
        };
        call
    }
}

impl std::ops::DerefMut for ToolCallAccumulatorMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let OutputItemAccumulator::ToolCall(call) = &mut self.state.output_items[self.output_index]
        else {
            unreachable!("tool-call guard owns a tool-call slot");
        };
        call
    }
}

impl Drop for ToolCallAccumulatorMut<'_> {
    fn drop(&mut self) {
        let new = slot_metrics(&self.state.output_items[self.output_index]);
        self.state.update_slot_metrics(self.old, new);
    }
}

/// Cached aggregate contributions from one provider output slot.
#[derive(Clone, Copy)]
struct SlotMetrics {
    /// Visible assistant UTF-8 bytes.
    assistant_bytes: u64,
    /// Non-visible tool-input UTF-8 bytes.
    non_visible_bytes: u64,
    /// Whether the slot makes transparent replay unsafe.
    semantic: bool,
    /// Whether the slot qualifies for first-output timing.
    timed: bool,
}

fn slot_metrics(item: &OutputItemAccumulator) -> SlotMetrics {
    match item {
        OutputItemAccumulator::Empty => SlotMetrics {
            assistant_bytes: 0,
            non_visible_bytes: 0,
            semantic: false,
            timed: false,
        },
        OutputItemAccumulator::Message(message) => {
            let present = !message.text.is_empty();
            SlotMetrics {
                assistant_bytes: message.text.len() as u64,
                non_visible_bytes: 0,
                semantic: present,
                timed: present,
            }
        }
        OutputItemAccumulator::ToolCall(call) => SlotMetrics {
            assistant_bytes: 0,
            non_visible_bytes: call.arguments_json.len() as u64,
            semantic: !call.id.is_empty()
                || !call.name.is_empty()
                || !call.arguments_json.is_empty(),
            timed: !call.name.is_empty() || !call.arguments_json.is_empty(),
        },
        OutputItemAccumulator::Reasoning(_) => SlotMetrics {
            assistant_bytes: 0,
            non_visible_bytes: 0,
            semantic: true,
            timed: true,
        },
        OutputItemAccumulator::Compaction(_) | OutputItemAccumulator::UnknownProviderItem(_) => {
            SlotMetrics {
                assistant_bytes: 0,
                non_visible_bytes: 0,
                semantic: true,
                timed: false,
            }
        }
    }
}

impl ToolCallAccumulator {
    pub(crate) fn new(tool_type: tau_proto::ToolType) -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            tool_type,
            arguments_json: String::new(),
            responses_envelope: ResponsesToolCallEnvelope::default(),
        }
    }

    fn context_item(&self) -> Option<ContextItem> {
        if self.name.is_empty() {
            return None;
        }
        let arguments = match self.tool_type {
            tau_proto::ToolType::Function => {
                let args: serde_json::Value =
                    serde_json::from_str(&self.arguments_json).unwrap_or(serde_json::Value::Null);
                json_to_cbor(&args)
            }
            tau_proto::ToolType::Custom => CborValue::Text(self.arguments_json.clone()),
        };
        let name = tau_proto::ToolName::try_new(self.name.clone())?;
        Some(ContextItem::ToolCall(ToolCallItem {
            call_id: self.id.clone().into(),
            name,
            tool_type: self.tool_type,
            arguments,
            raw_arguments_json: (self.tool_type == tau_proto::ToolType::Function)
                .then(|| self.arguments_json.clone()),
            responses_envelope: (!self.responses_envelope.is_empty())
                .then(|| self.responses_envelope.clone()),
        }))
    }

    fn into_context_item(self) -> Option<ContextItem> {
        if self.name.is_empty() {
            return None;
        }
        let Self {
            id,
            name,
            tool_type,
            arguments_json,
            responses_envelope,
        } = self;
        let (arguments, raw_arguments_json) = match tool_type {
            tau_proto::ToolType::Function => {
                let args: serde_json::Value =
                    serde_json::from_str(&arguments_json).unwrap_or(serde_json::Value::Null);
                (json_to_cbor(&args), Some(arguments_json))
            }
            tau_proto::ToolType::Custom => (CborValue::Text(arguments_json), None),
        };
        let name = tau_proto::ToolName::try_new(name)?;
        Some(ContextItem::ToolCall(ToolCallItem {
            call_id: id.into(),
            name,
            tool_type,
            arguments,
            raw_arguments_json,
            responses_envelope: (!responses_envelope.is_empty()).then_some(responses_envelope),
        }))
    }
}

impl OutputItemAccumulator {
    fn context_item(&self) -> Option<ContextItem> {
        match self {
            OutputItemAccumulator::Empty => None,
            OutputItemAccumulator::Message(message) => (!message.text.is_empty()).then(|| {
                assistant_text_item_with_phase_and_raw(
                    message.text.clone(),
                    message.phase,
                    message.responses_raw_json.clone(),
                    message.citations.clone(),
                )
            }),
            OutputItemAccumulator::ToolCall(call) => call.context_item(),
            OutputItemAccumulator::Reasoning(item) => Some(ContextItem::Reasoning(item.clone())),
            OutputItemAccumulator::Compaction(Some(item)) => {
                Some(ContextItem::Compaction(item.clone()))
            }
            OutputItemAccumulator::Compaction(None) => None,
            OutputItemAccumulator::UnknownProviderItem(item) => {
                Some(ContextItem::UnknownProviderItem(item.clone()))
            }
        }
    }

    fn into_context_item(self) -> Option<ContextItem> {
        match self {
            Self::Empty | Self::Compaction(None) => None,
            Self::Message(message) => (!message.text.is_empty()).then(|| {
                assistant_text_item_with_phase_and_raw(
                    message.text,
                    message.phase,
                    message.responses_raw_json,
                    message.citations,
                )
            }),
            Self::ToolCall(call) => call.into_context_item(),
            Self::Reasoning(item) => Some(ContextItem::Reasoning(item)),
            Self::Compaction(Some(item)) => Some(ContextItem::Compaction(item)),
            Self::UnknownProviderItem(item) => Some(ContextItem::UnknownProviderItem(item)),
        }
    }

    fn materializes_context_item(&self) -> bool {
        match self {
            OutputItemAccumulator::Empty | OutputItemAccumulator::Compaction(None) => false,
            OutputItemAccumulator::Message(message) => !message.text.is_empty(),
            OutputItemAccumulator::ToolCall(call) => {
                call.name.len() <= tau_proto::ToolName::MAX_LEN
                    && tau_proto::ToolName::try_new(call.name.clone()).is_some()
            }
            OutputItemAccumulator::Reasoning(_)
            | OutputItemAccumulator::Compaction(Some(_))
            | OutputItemAccumulator::UnknownProviderItem(_) => true,
        }
    }
}

impl Default for StreamState {
    fn default() -> Self {
        Self::new()
    }
}

fn add_len(current: u64, len: usize) -> u64 {
    current.saturating_add(u64::try_from(len).unwrap_or(u64::MAX))
}

fn add_optional_string(current: u64, value: Option<&String>) -> u64 {
    value.map_or(current, |value| add_len(current, value.len()))
}

fn add_serialized<T: serde::Serialize>(current: u64, value: Option<&T>) -> u64 {
    value.map_or(current, |value| {
        serde_json::to_vec(value).map_or(u64::MAX, |value| add_len(current, value.len()))
    })
}

impl StreamState {
    /// Construct an empty provider response accumulator.
    pub(crate) fn new() -> Self {
        Self {
            output_items: Vec::new(),
            active_web_searches: BTreeMap::new(),
            web_search_lifecycle: None,
            web_search_lifecycle_revision: 0,
            assistant_text_bytes: 0,
            non_visible_output_bytes: 0,
            semantic_output_items: 0,
            timed_output_items: 0,
            input_tokens: None,
            cached_tokens: None,
            cache_write_tokens: None,
            prompt_cache_read_ceiling_tokens: None,
            output_tokens: None,
            thinking: None,
            thinking_output_index: None,
            response_id: None,
            provider_terminal_event: None,
            transport_response_bytes: 0,
            stale_chain_fallback: false,
            repetition_guard: StreamRepetitionGuard::new(),
            quota_observation: None,
            provider_evidence_mode: path_crate_attempt_failure::ProviderEvidenceMode::LiveOnly,
            retained_state_bytes: 0,
            #[cfg(test)]
            aggregate_assistant_text_copied_bytes: Cell::new(0),
        }
    }

    /// Returns the logical bytes retained by provider semantic state.
    ///
    /// The measure charges every output slot at its inline Rust size and
    /// charges each retained string or structured replay value at its
    /// serialized byte size. It is an admission model rather than an
    /// allocator-RSS estimate.
    pub(crate) fn logical_retained_bytes(&self) -> u64 {
        let slots = self
            .output_items
            .len()
            .saturating_mul(std::mem::size_of::<OutputItemAccumulator>());
        let mut bytes = u64::try_from(slots).unwrap_or(u64::MAX);
        // Preserve the admission charge formerly owned by the duplicate
        // concatenated assistant-text allocation without retaining that copy.
        bytes = bytes.saturating_add(self.assistant_text_bytes);
        bytes = add_optional_string(bytes, self.thinking.as_ref());
        bytes = add_optional_string(bytes, self.response_id.as_ref());
        bytes = add_serialized(bytes, self.provider_terminal_event.as_ref());
        for item in &self.output_items {
            bytes = bytes.saturating_add(output_item_retained_payload_bytes(item));
        }
        bytes
    }

    /// Returns the live owner's cached retained-state admission total.
    pub(crate) fn admitted_retained_state_bytes(&self) -> u64 {
        self.retained_state_bytes
    }

    /// Commits one preflighted retained-state admission total.
    pub(crate) fn commit_retained_state_bytes(&mut self, bytes: u64) {
        self.retained_state_bytes = bytes;
    }

    /// Returns the latest normalized quota observation on this turn.
    #[must_use]
    pub fn quota_observation(&self) -> Option<&crate::quota::RollingQuotaObservation> {
        self.quota_observation.as_ref()
    }

    /// Returns whether success followed canonical stale-chain recovery.
    #[must_use]
    pub fn stale_chain_fallback(&self) -> bool {
        self.stale_chain_fallback
    }

    /// Returns input, cached-input, and output token counters.
    #[must_use]
    pub fn token_counts(&self) -> ProviderTokenCounts {
        ProviderTokenCounts {
            input: self.input_tokens,
            cached: self.cached_tokens,
            output: self.output_tokens,
        }
    }

    /// Returns the provider response id for durable chain metadata.
    #[must_use]
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    /// Constructs synthetic terminal facts for compatibility snapshot tests.
    #[must_use]
    #[cfg(feature = "test-support")]
    pub fn with_terminal_facts(
        mut self,
        input_tokens: u64,
        cached_tokens: u64,
        output_tokens: u64,
        response_id: String,
    ) -> Self {
        self.input_tokens = Some(input_tokens);
        self.cached_tokens = Some(cached_tokens);
        self.output_tokens = Some(output_tokens);
        self.response_id = Some(response_id);
        self
    }

    pub(crate) fn check_message_delta(
        &mut self,
        output_index: usize,
        delta: &str,
    ) -> Result<(), LlmError> {
        if let Some(repetition) = self
            .repetition_guard
            .push_delta(StreamRepetitionKey::AssistantText { output_index }, delta)
        {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        Ok(())
    }

    pub(crate) fn check_message_snapshot(
        &mut self,
        output_index: usize,
        text: &str,
    ) -> Result<(), LlmError> {
        let current = self.message_text_at(output_index).unwrap_or_default();
        if let Some(delta) = text.strip_prefix(current) {
            self.check_message_delta(output_index, delta)
        } else if let Some(repetition) = self
            .repetition_guard
            .replace_tail(StreamRepetitionKey::AssistantText { output_index }, text)
        {
            Err(LlmError::RepetitionDetected(repetition))
        } else {
            Ok(())
        }
    }

    pub(crate) fn check_reasoning_delta(
        &mut self,
        output_index: usize,
        delta: &str,
    ) -> Result<(), LlmError> {
        if let Some(repetition) = self
            .repetition_guard
            .push_delta(StreamRepetitionKey::ReasoningText { output_index }, delta)
        {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        Ok(())
    }

    pub(crate) fn check_function_arguments_delta(
        &mut self,
        output_index: usize,
        delta: &str,
    ) -> Result<(), LlmError> {
        if let Some(repetition) = self.repetition_guard.push_delta(
            StreamRepetitionKey::FunctionCallArguments { output_index },
            delta,
        ) {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        Ok(())
    }

    pub(crate) fn check_function_arguments_snapshot(
        &mut self,
        output_index: usize,
        text: &str,
    ) -> Result<(), LlmError> {
        let current = self.tool_arguments_at(output_index).unwrap_or_default();
        if let Some(delta) = text.strip_prefix(current) {
            self.check_function_arguments_delta(output_index, delta)
        } else if let Some(repetition) = self.repetition_guard.replace_tail(
            StreamRepetitionKey::FunctionCallArguments { output_index },
            text,
        ) {
            Err(LlmError::RepetitionDetected(repetition))
        } else {
            Ok(())
        }
    }

    pub(crate) fn check_custom_tool_input_delta(
        &mut self,
        output_index: usize,
        delta: &str,
    ) -> Result<(), LlmError> {
        if let Some(repetition) = self
            .repetition_guard
            .push_delta(StreamRepetitionKey::CustomToolInput { output_index }, delta)
        {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        Ok(())
    }

    pub(crate) fn check_custom_tool_input_snapshot(
        &mut self,
        output_index: usize,
        text: &str,
    ) -> Result<(), LlmError> {
        let current = self.tool_arguments_at(output_index).unwrap_or_default();
        if let Some(delta) = text.strip_prefix(current) {
            self.check_custom_tool_input_delta(output_index, delta)
        } else if let Some(repetition) = self
            .repetition_guard
            .replace_tail(StreamRepetitionKey::CustomToolInput { output_index }, text)
        {
            Err(LlmError::RepetitionDetected(repetition))
        } else {
            Ok(())
        }
    }

    fn ensure_output_len(&mut self, output_index: usize) {
        while self.output_items.len() <= output_index {
            self.output_items.push(OutputItemAccumulator::Empty);
        }
    }

    /// Reserves an output-item slot without committing a durable item yet.
    pub(crate) fn reserve_output_item_at(&mut self, output_index: usize) {
        self.ensure_output_len(output_index);
    }

    fn message_at_mut(&mut self, output_index: usize) -> &mut MessageAccumulator {
        self.ensure_output_len(output_index);
        if !matches!(
            self.output_items[output_index],
            OutputItemAccumulator::Message(_)
        ) {
            self.replace_output_item(
                output_index,
                OutputItemAccumulator::Message(MessageAccumulator::default()),
            );
        }
        let OutputItemAccumulator::Message(message) = &mut self.output_items[output_index] else {
            unreachable!("message slot was just initialized");
        };
        message
    }

    fn message_text_at(&self, output_index: usize) -> Option<&str> {
        match self.output_items.get(output_index) {
            Some(OutputItemAccumulator::Message(message)) => Some(&message.text),
            _ => None,
        }
    }

    fn tool_arguments_at(&self, output_index: usize) -> Option<&str> {
        match self.output_items.get(output_index) {
            Some(OutputItemAccumulator::ToolCall(call)) => Some(&call.arguments_json),
            _ => None,
        }
    }

    pub(crate) fn append_message_delta_at(&mut self, output_index: usize, delta: &str) {
        self.message_at_mut(output_index);
        let old = slot_metrics(&self.output_items[output_index]);
        let OutputItemAccumulator::Message(message) = &mut self.output_items[output_index] else {
            unreachable!("message slot was just initialized");
        };
        message.text.push_str(delta);
        self.update_slot_metrics(old, slot_metrics(&self.output_items[output_index]));
    }

    pub(crate) fn set_message_text_at(&mut self, output_index: usize, text: &str) {
        self.message_at_mut(output_index);
        let old = slot_metrics(&self.output_items[output_index]);
        let OutputItemAccumulator::Message(message) = &mut self.output_items[output_index] else {
            unreachable!("message slot was just initialized");
        };
        message.text = text.to_owned();
        self.update_slot_metrics(old, slot_metrics(&self.output_items[output_index]));
    }

    pub(crate) fn set_message_phase_at(
        &mut self,
        output_index: usize,
        phase: Option<tau_proto::MessagePhase>,
    ) {
        if let Some(phase) = phase {
            self.message_at_mut(output_index).phase = Some(phase);
        }
    }

    /// Stores the raw Responses assistant message item for one output index.
    pub(crate) fn set_message_responses_raw_json_at(
        &mut self,
        output_index: usize,
        raw_json: Option<&str>,
    ) {
        self.message_at_mut(output_index).responses_raw_json = raw_json.map(str::to_owned);
    }

    /// Replace one message slot's validated semantic citation metadata.
    pub(crate) fn set_message_citations_at(
        &mut self,
        output_index: usize,
        citations: Vec<ContentPart>,
    ) {
        self.message_at_mut(output_index).citations = citations;
    }

    pub(crate) fn tool_call_at_mut(
        &mut self,
        output_index: usize,
        tool_type: tau_proto::ToolType,
    ) -> ToolCallAccumulatorMut<'_> {
        self.ensure_output_len(output_index);
        if !matches!(
            self.output_items[output_index],
            OutputItemAccumulator::ToolCall(_)
        ) {
            self.replace_output_item(
                output_index,
                OutputItemAccumulator::ToolCall(ToolCallAccumulator::new(tool_type)),
            );
        }
        let old = slot_metrics(&self.output_items[output_index]);
        let mut guard = ToolCallAccumulatorMut {
            state: self,
            output_index,
            old,
        };
        guard.tool_type = tool_type;
        guard
    }

    pub(crate) fn set_reasoning_item_at(
        &mut self,
        output_index: usize,
        item: &serde_json::Value,
        raw_json: String,
    ) {
        self.ensure_output_len(output_index);
        self.replace_output_item(
            output_index,
            OutputItemAccumulator::Reasoning(opaque_item_from_value(item, raw_json)),
        );
    }

    pub(crate) fn start_compaction_item_at(&mut self, output_index: usize) {
        self.ensure_output_len(output_index);
        if !matches!(
            self.output_items[output_index],
            OutputItemAccumulator::Compaction(_)
        ) {
            self.replace_output_item(output_index, OutputItemAccumulator::Compaction(None));
        }
    }

    pub(crate) fn set_compaction_item_at(
        &mut self,
        output_index: usize,
        item: &serde_json::Value,
        raw_json: String,
    ) {
        self.ensure_output_len(output_index);
        self.replace_output_item(
            output_index,
            OutputItemAccumulator::Compaction(Some(opaque_item_from_value(item, raw_json))),
        );
    }

    /// Stores an unrecognized provider output item at its provider index.
    pub(crate) fn set_unknown_provider_item_at(
        &mut self,
        output_index: usize,
        item: &serde_json::Value,
        raw_json: String,
    ) {
        self.ensure_output_len(output_index);
        self.replace_output_item(
            output_index,
            OutputItemAccumulator::UnknownProviderItem(opaque_item_from_value(item, raw_json)),
        );
    }

    pub(crate) fn set_web_search_active(
        &mut self,
        output_index: usize,
        call_id: String,
        active: bool,
    ) {
        if active {
            self.active_web_searches
                .insert(output_index, call_id.clone());
        } else {
            self.active_web_searches.remove(&output_index);
        }
        self.web_search_lifecycle_revision = self.web_search_lifecycle_revision.saturating_add(1);
        self.web_search_lifecycle = Some((self.web_search_lifecycle_revision, call_id, active));
    }

    /// Whether at least one provider-hosted web search is currently active.
    #[must_use]
    pub fn web_search_active(&self) -> bool {
        !self.active_web_searches.is_empty()
    }

    /// Returns the latest hosted web-search lifecycle transition.
    #[must_use]
    pub fn web_search_lifecycle(&self) -> Option<(u64, &str, bool)> {
        self.web_search_lifecycle
            .as_ref()
            .map(|(revision, call_id, active)| (*revision, call_id.as_str(), *active))
    }

    /// Appends displayable reasoning-summary text at the provider output index
    /// it belongs to.
    pub(crate) fn append_reasoning_summary_delta_at(&mut self, output_index: usize, delta: &str) {
        self.thinking_output_index.get_or_insert(output_index);
        self.thinking
            .get_or_insert_with(String::new)
            .push_str(delta);
    }

    /// Starts a new reasoning-summary paragraph at the provider output index
    /// it belongs to.
    pub(crate) fn start_reasoning_summary_part_at(&mut self, output_index: usize) {
        self.thinking_output_index.get_or_insert(output_index);
        if let Some(thinking) = self.thinking.as_mut()
            && !thinking.is_empty()
            && !thinking.ends_with("\n\n")
        {
            thinking.push_str("\n\n");
        }
    }

    /// Returns the cumulative non-visible provider output bytes generated for
    /// this prompt, such as streamed tool-call arguments and custom-tool input.
    pub fn non_visible_output_bytes(&self) -> u64 {
        self.non_visible_output_bytes
    }

    /// Returns cumulative visible assistant UTF-8 bytes without materializing
    /// the provider-index-ordered aggregate.
    pub(crate) fn assistant_text_bytes(&self) -> u64 {
        self.assistant_text_bytes
    }

    /// Returns the cumulative provider response-progress bytes for this
    /// response, preferring transport-received bytes so progress moves before
    /// provider payloads have been semantically parsed.
    pub fn response_bytes_received(&self) -> u64 {
        let visible_bytes = self.assistant_text_bytes.saturating_add(
            self.thinking
                .as_ref()
                .map_or(0, |thinking| thinking.len() as u64),
        );
        visible_bytes
            .saturating_add(self.non_visible_output_bytes())
            .max(self.transport_response_bytes)
    }

    /// Returns whether this attempt parsed any model-semantic output.
    ///
    /// Transport bytes, quota telemetry, response ids, and usage are
    /// deliberately excluded: none of them make replay unsafe. Any material
    /// provider output item or reasoning text does.
    #[must_use]
    pub fn has_semantic_progress(&self) -> bool {
        self.semantic_output_items != 0
            || self
                .thinking
                .as_ref()
                .is_some_and(|thinking| !thinking.is_empty())
    }

    /// Returns whether accepted state contains output that qualifies for the
    /// provider-owned first-semantic-output timer.
    ///
    /// Unlike replay-safety progress, this excludes call ids, compaction, and
    /// unknown provider items.
    #[must_use]
    pub fn has_timed_semantic_output(&self) -> bool {
        self.timed_output_items != 0
            || self
                .thinking
                .as_ref()
                .is_some_and(|thinking| !thinking.is_empty())
    }

    /// Adds raw bytes received from the provider transport before semantic
    /// parsing.
    pub(crate) fn record_transport_response_bytes(&mut self, bytes: usize) {
        self.transport_response_bytes = self
            .transport_response_bytes
            .saturating_add(bytes.try_into().unwrap_or(u64::MAX));
    }

    /// Returns exact cumulative transport bytes, including discarded repair.
    pub(crate) fn transport_response_bytes(&self) -> u64 {
        self.transport_response_bytes
    }

    /// Carries bytes from a discarded transport-repair attempt into this state.
    pub(crate) fn carry_transport_response_bytes(&mut self, bytes: u64) {
        self.transport_response_bytes = self.transport_response_bytes.saturating_add(bytes);
    }

    /// Materializes provider-index-ordered assistant text for terminal
    /// fallback.
    ///
    /// Live streaming paths must use indexed deltas and cached byte totals;
    /// calling this after each delta would restore cumulative quadratic work.
    pub(crate) fn aggregate_assistant_text(&self) -> String {
        let capacity = usize::try_from(self.assistant_text_bytes).unwrap_or(usize::MAX);
        let mut text = String::with_capacity(capacity);
        for item in &self.output_items {
            if let OutputItemAccumulator::Message(message) = item {
                text.push_str(&message.text);
                #[cfg(test)]
                self.aggregate_assistant_text_copied_bytes.set(
                    self.aggregate_assistant_text_copied_bytes
                        .get()
                        .saturating_add(message.text.len() as u64),
                );
            }
        }
        text
    }

    /// Returns deterministic assistant aggregate-copy work for scaling tests.
    #[cfg(test)]
    pub(crate) fn aggregate_assistant_text_copied_bytes(&self) -> u64 {
        self.aggregate_assistant_text_copied_bytes.get()
    }

    fn replace_output_item(&mut self, output_index: usize, item: OutputItemAccumulator) {
        let old = slot_metrics(&self.output_items[output_index]);
        self.output_items[output_index] = item;
        let new = slot_metrics(&self.output_items[output_index]);
        self.update_slot_metrics(old, new);
    }

    fn update_slot_metrics(&mut self, old: SlotMetrics, new: SlotMetrics) {
        self.assistant_text_bytes = self
            .assistant_text_bytes
            .saturating_sub(old.assistant_bytes)
            .saturating_add(new.assistant_bytes);
        self.non_visible_output_bytes = self
            .non_visible_output_bytes
            .saturating_sub(old.non_visible_bytes)
            .saturating_add(new.non_visible_bytes);
        self.semantic_output_items = self
            .semantic_output_items
            .saturating_sub(usize::from(old.semantic))
            .saturating_add(usize::from(new.semantic));
        self.timed_output_items = self
            .timed_output_items
            .saturating_sub(usize::from(old.timed))
            .saturating_add(usize::from(new.timed));
    }

    /// Returns the current compact compaction status, when a compaction item is
    /// present in the live provider output.
    pub fn compaction_update(&self) -> Option<ProviderResponseCompactionUpdate> {
        self.output_items.iter().find_map(|item| match item {
            OutputItemAccumulator::Compaction(Some(_)) => Some(ProviderResponseCompactionUpdate {
                status: ProviderResponseCompactionStatus::Completed,
                original_input_tokens: None,
                compaction_output_tokens: None,
            }),
            OutputItemAccumulator::Compaction(None) => Some(ProviderResponseCompactionUpdate {
                status: ProviderResponseCompactionStatus::Started,
                original_input_tokens: None,
                compaction_output_tokens: None,
            }),
            _ => None,
        })
    }

    /// Clones the final assistant output items for transport bookkeeping.
    ///
    /// This uses the same materialization as [`Self::into_output_items`] so a
    /// response-chain anchor fingerprints exactly what the harness persists.
    pub(crate) fn output_items_snapshot(&self) -> Vec<ContextItem> {
        let mut items = Vec::new();
        let thinking_index = self.thinking_output_index.unwrap_or(0);
        let thinking = self
            .thinking
            .as_ref()
            .filter(|thinking| !thinking.is_empty());
        let output_items = self
            .output_items
            .iter()
            .map(|item| item.context_item())
            .collect::<Vec<_>>();
        let thinking_len = thinking.as_ref().map(|_| thinking_index + 1).unwrap_or(0);
        let len = output_items.len().max(thinking_len);

        for index in 0..len {
            if index == thinking_index
                && let Some(thinking) = &thinking
            {
                items.push(ContextItem::ReasoningText(ReasoningTextItem {
                    kind: ReasoningTextKind::Summary,
                    text: (*thinking).clone(),
                }));
            }
            if let Some(item) = output_items.get(index).and_then(Option::as_ref) {
                items.push(item.clone());
            }
        }

        if items.is_empty() && self.assistant_text_bytes != 0 {
            items.push(assistant_text_item(self.aggregate_assistant_text()));
        }

        items
    }

    /// Borrows the sole completed compaction item after validating the exact
    /// compact-response output shape.
    pub(crate) fn single_compaction_item(&self) -> Option<&OpaqueProviderItem> {
        if self
            .thinking
            .as_ref()
            .is_some_and(|thinking| !thinking.is_empty())
        {
            return None;
        }
        let mut compaction = None;
        for item in &self.output_items {
            match item {
                OutputItemAccumulator::Compaction(Some(item)) if compaction.is_none() => {
                    compaction = Some(item);
                }
                item if !item.materializes_context_item() => {}
                _ => return None,
            }
        }
        compaction
    }

    /// Temporarily presents the sole validated compaction output as its
    /// canonical context item without cloning the opaque provider sidecar.
    pub(crate) fn with_single_compaction_context_item<R>(
        &mut self,
        inspect: impl FnOnce(&ContextItem) -> R,
    ) -> Option<R> {
        self.single_compaction_item()?;
        let index = self
            .output_items
            .iter()
            .position(|item| matches!(item, OutputItemAccumulator::Compaction(Some(_))))?;
        let OutputItemAccumulator::Compaction(Some(item)) =
            std::mem::replace(&mut self.output_items[index], OutputItemAccumulator::Empty)
        else {
            unreachable!("validated compaction slot changed before extraction");
        };
        let context_item = ContextItem::Compaction(item);
        let result = inspect(&context_item);
        let ContextItem::Compaction(item) = context_item else {
            unreachable!("temporary compaction context item changed variant");
        };
        self.output_items[index] = OutputItemAccumulator::Compaction(Some(item));
        Some(result)
    }

    /// Returns the final assistant output items in provider item order.
    ///
    /// Tool-call accumulators with an empty `name` are dropped as stream
    /// artifacts. The streaming paths eagerly create slots from
    /// argument-delta events so the index stays addressable; if the
    /// matching name-carrying event never arrives, shipping it
    /// downstream would surface as an `invalid_tool` rejection in the
    /// harness even though the model never committed a valid call.
    pub fn into_output_items(self) -> Vec<ContextItem> {
        let aggregate_fallback = (self.assistant_text_bytes != 0
            && !self
                .output_items
                .iter()
                .any(OutputItemAccumulator::materializes_context_item))
        .then(|| self.aggregate_assistant_text());
        let thinking_index = self.thinking_output_index.unwrap_or(0);
        let mut thinking = self.thinking.filter(|thinking| !thinking.is_empty());
        let output_items = self
            .output_items
            .into_iter()
            .map(OutputItemAccumulator::into_context_item)
            .collect::<Vec<_>>();
        let thinking_len = thinking.as_ref().map(|_| thinking_index + 1).unwrap_or(0);
        let len = output_items.len().max(thinking_len);
        let mut items = Vec::new();

        let mut output_items = output_items.into_iter();
        for index in 0..len {
            if index == thinking_index
                && let Some(thinking) = thinking.take()
            {
                items.push(ContextItem::ReasoningText(ReasoningTextItem {
                    kind: ReasoningTextKind::Summary,
                    text: thinking,
                }));
            }
            if let Some(item) = output_items.next().flatten() {
                items.push(item);
            }
        }
        if items.is_empty()
            && let Some(text) = aggregate_fallback
        {
            items.push(assistant_text_item(text));
        }
        items
    }

    /// Extracts exactly one completed compaction item without cloning retained
    /// provider output.
    ///
    /// Empty streaming artifacts retain the same projection semantics as
    /// [`Self::into_output_items`]. Any other projected item or non-empty
    /// reasoning summary rejects the compact output before materialization.
    pub(crate) fn into_single_compaction_item(self) -> Option<OpaqueProviderItem> {
        self.single_compaction_item()?;
        self.output_items.into_iter().find_map(|item| match item {
            OutputItemAccumulator::Compaction(Some(item)) => Some(item),
            _ => None,
        })
    }

    /// Returns response-local usage when the provider supplied any usage field,
    /// preserving an explicitly reported all-zero record.
    pub fn usage(&self) -> Option<ProviderTokenUsage> {
        if self.input_tokens.is_none()
            && self.cached_tokens.is_none()
            && self.output_tokens.is_none()
        {
            return None;
        }
        let input = self.input_tokens.unwrap_or(0);
        let cached = self.cached_tokens.unwrap_or(0).min(input);
        let output = self.output_tokens.unwrap_or(0);
        let read_tokens = self.cached_tokens.map(|tokens| tokens.min(input));
        let cache = (read_tokens.is_some()
            || self.cache_write_tokens.is_some()
            || self.prompt_cache_read_ceiling_tokens.is_some())
        .then(|| {
            Box::new(
                tau_proto::ProviderCacheUsage {
                    read_tokens,
                    write_tokens: self.cache_write_tokens,
                    miss_tokens: None,
                    cacheable_prefix_tokens: self.prompt_cache_read_ceiling_tokens,
                    refresh_reason: Some(tau_proto::ProviderCacheRefreshReason::OrdinaryRequest),
                    expiry_confidence: Some(tau_proto::ProviderCacheExpiryConfidence::Unknown),
                    avoided_prefill_tokens: read_tokens,
                    storage_token_micros: None,
                }
                .normalized(input),
            )
        });
        Some(ProviderTokenUsage {
            model: None,
            prompt_sent_tokens: input,
            prompt_cached_tokens: cached,
            prompt_cache_read_ceiling_tokens: (!self.stale_chain_fallback)
                .then_some(self.prompt_cache_read_ceiling_tokens)
                .flatten(),
            cache,
            response_received_tokens: output,
            stats: Default::default(),
        })
    }
}

/// Returns logical retained bytes inside one output slot, excluding the slot.
pub(crate) fn output_item_retained_payload_bytes(item: &OutputItemAccumulator) -> u64 {
    match item {
        OutputItemAccumulator::Empty | OutputItemAccumulator::Compaction(None) => 0,
        OutputItemAccumulator::Message(message) => add_optional_string(
            message.text.len() as u64,
            message.responses_raw_json.as_ref(),
        ),
        OutputItemAccumulator::ToolCall(call) => {
            let bytes = add_len(0, call.id.len());
            let bytes = add_len(bytes, call.name.len());
            let bytes = add_len(bytes, call.arguments_json.len());
            add_serialized(bytes, Some(&call.responses_envelope))
        }
        OutputItemAccumulator::Reasoning(item)
        | OutputItemAccumulator::Compaction(Some(item))
        | OutputItemAccumulator::UnknownProviderItem(item) => {
            let bytes = add_serialized(0, Some(item.value()));
            add_len(bytes, item.raw_json().len())
        }
    }
}

pub fn assistant_text_item(text: impl Into<String>) -> ContextItem {
    assistant_text_item_with_phase(text.into(), None)
}

pub fn assistant_text_item_with_phase(
    text: impl Into<String>,
    phase: Option<tau_proto::MessagePhase>,
) -> ContextItem {
    assistant_text_item_with_phase_and_raw(text, phase, None, Vec::new())
}

/// Builds an assistant text item with optional Responses replay sidecar.
pub fn assistant_text_item_with_phase_and_raw(
    text: impl Into<String>,
    phase: Option<tau_proto::MessagePhase>,
    responses_raw_json: Option<String>,
    citations: Vec<ContentPart>,
) -> ContextItem {
    let mut content = vec![ContentPart::Text { text: text.into() }];
    content.extend(citations);
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content,
        phase,
        responses_raw_json,
    })
}

fn opaque_item_from_value(item: &serde_json::Value, raw_json: String) -> OpaqueProviderItem {
    // `raw_json` was sliced from the same successfully parsed provider event as
    // `item`; disagreement here is an internal extraction bug, not provider
    // input validation. Keep the assertion explicit so that invariant remains
    // auditable at this construction boundary.
    OpaqueProviderItem::try_new(json_to_cbor(item), raw_json)
        .expect("parsed provider item must match its retained raw JSON")
}

/// Maps `NativeReasoningEffort` to the wire string the OpenAI Responses /
/// Chat Completions APIs accept. `Off` maps to OpenAI's explicit
/// `none` so provider defaults (for example GPT-5.5's `medium`) do
/// not silently apply.
pub fn effort_wire(level: tau_proto::NativeReasoningEffort) -> Option<&'static str> {
    use tau_proto::NativeReasoningEffort::*;
    match level {
        None => Some("none"),
        Minimal => Some("minimal"),
        Low => Some("low"),
        Medium => Some("medium"),
        High => Some("high"),
        XHigh => Some("xhigh"),
        Max => Some("max"),
    }
}

/// Maps `Verbosity` to the wire string OpenAI's `verbosity` /
/// `text.verbosity` field accepts. There is no "off" sentinel — the
/// caller gates the field on a provider-level `supports_verbosity`
/// flag instead.
pub fn verbosity_wire(level: tau_proto::Verbosity) -> &'static str {
    level.as_openai_wire()
}

/// Derive the wire `prompt_cache_key` for the OpenAI-style provider cache.
///
/// The resulting UUID is version 8 shaped from a deterministic hash of the
/// provider endpoint, protocol identity, and durable agent lifetime. Prompt
/// provenance/originator is intentionally excluded so agent-to-agent messages,
/// manager relays, and direct user prompts keep the target agent on the same
/// provider cache bucket.
pub fn prompt_cache_key_for(
    base_url: &str,
    agent_id: &tau_proto::AgentId,
    mode: crate::responses::ResponsesMode,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(base_url.as_bytes());
    hasher.update(b"protocol:");
    hasher.update(mode.cache_identity().as_bytes());
    hasher.update(b"agent:");
    hasher.update(agent_id.as_str().as_bytes());

    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    Uuid::new_v8(bytes).to_string()
}

// ---------------------------------------------------------------------------
// CBOR ↔ JSON value conversion
// ---------------------------------------------------------------------------

pub fn cbor_to_json(v: &CborValue) -> serde_json::Value {
    match v {
        CborValue::Null => serde_json::Value::Null,
        CborValue::Bool(b) => serde_json::Value::Bool(*b),
        CborValue::Integer(i) => {
            let n: i128 = (*i).into();
            serde_json::json!(n)
        }
        CborValue::Float(f) => serde_json::json!(f),
        CborValue::Text(s) => serde_json::Value::String(s.clone()),
        CborValue::Bytes(bytes) => serde_json::Value::String(base64::Engine::encode(
            &path_base64_engine::general_purpose::STANDARD,
            bytes,
        )),
        CborValue::Array(arr) => serde_json::Value::Array(arr.iter().map(cbor_to_json).collect()),
        CborValue::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                let key = match k {
                    CborValue::Text(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                map.insert(key, cbor_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        CborValue::Tag(_, inner) => cbor_to_json(inner),
        other => {
            tracing::warn!(target: crate::LOG_TARGET, "unsupported CBOR value in tool input: {other:?}");
            serde_json::Value::Null
        }
    }
}

pub fn json_to_cbor(v: &serde_json::Value) -> CborValue {
    match v {
        serde_json::Value::Null => CborValue::Null,
        serde_json::Value::Bool(b) => CborValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CborValue::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                CborValue::Integer(u.into())
            } else if let Some(f) = n.as_f64() {
                CborValue::Float(f)
            } else {
                CborValue::Null
            }
        }
        serde_json::Value::String(s) => CborValue::Text(s.clone()),
        serde_json::Value::Array(arr) => CborValue::Array(arr.iter().map(json_to_cbor).collect()),
        serde_json::Value::Object(map) => CborValue::Map(
            map.iter()
                .map(|(k, v)| (CborValue::Text(k.clone()), json_to_cbor(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests;
