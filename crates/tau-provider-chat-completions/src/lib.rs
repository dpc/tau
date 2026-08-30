//! OpenAI-compatible Chat Completions backend helpers.
//!
//! Component responsibilities and provider/replay trust boundaries are
//! documented in `ARCH-tau-provider-chat-completions`.

mod canonical_identifier;
mod compact_stream;

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
#[cfg(test)]
use std::io::Read;
use std::time::{Duration, Instant, SystemTime};
use std::{cell as path_std_cell, io as path_std_io};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use canonical_identifier::CanonicalIdentifierFamily;
use serde::Serialize;
#[cfg(test)]
use tau_proto::ToolResultStatus;
use tau_proto::{
    ContentPart, ContextItem, ContextRole, ModelName, OpaqueProviderItem, ProviderStopReason,
    ProviderTokenUsage, ReasoningTextItem, ReasoningTextKind, ToolCallItem, ToolChoice,
    ToolDefinition, ToolType,
};
use tau_provider::retry_policy::{
    RetryClass, RetryDecision, classify_error_code, parse_json_reset_hint, parse_retry_after,
};
use tau_provider::{
    StreamRepetitionGuard, StreamRepetitionKey,
    debug_capture_writer as path_tau_provider_debug_capture_writer,
    private_attempt_trace as private_trace,
};
use tokio::runtime as path_tokio_runtime;

const LOG_TARGET: &str = "provider-chat-completions";

#[cfg(test)]
thread_local! {
    /// Attempt-path capture sink used only by focused transport tests.
    static TEST_DEBUG_CAPTURES: std::cell::RefCell<Option<Vec<path_tau_provider_debug_capture_writer::ProviderDebugCapture>>> =
        const { std::cell::RefCell::new(None) };
    /// Attempt-local deadline limits used by focused production-path tests.
    static TEST_REQUEST_DEADLINE_LIMITS: Cell<Option<(Duration, Duration)>> =
        const { Cell::new(None) };
    /// Deterministic request clock used by focused production-path tests.
    static TEST_REQUEST_NOW: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Submit one private capture through production transport or the focused test
/// sink.
fn submit_provider_capture(capture: path_tau_provider_debug_capture_writer::ProviderDebugCapture) {
    #[cfg(test)]
    let mut capture = Some(capture);
    #[cfg(test)]
    if TEST_DEBUG_CAPTURES.with(|captures| {
        let mut captures = captures.borrow_mut();
        captures.as_mut().is_some_and(|sink| {
            sink.push(capture.take().expect("capture is submitted once"));
            true
        })
    }) {
        return;
    }
    #[cfg(test)]
    let capture = capture.expect("test sink did not consume capture");
    tau_provider::debug_capture_writer::submit_provider_debug_capture(capture);
}
/// Default Chat Completions output-token cap Tau sends when no
/// provider-specific override is set.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;
const STREAM_READ_POLL_TIMEOUT: Duration = Duration::from_secs(1);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const STREAM_ABSOLUTE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
const MAX_DEBUG_EVENTS: usize = 4096;
const MAX_HTTP_ERROR_BODY_BYTES: u64 = 64 * 1024;
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REQUEST_IMAGE_BYTES: usize = 24 * 1024 * 1024;
const MAX_REQUEST_IMAGE_DATA_URL_BYTES: usize = 32 * 1024 * 1024;

/// Read the monotonic clock used only for request lifetime enforcement.
fn request_now() -> Instant {
    #[cfg(test)]
    if let Some(now) = TEST_REQUEST_NOW.with(Cell::get) {
        return now;
    }
    Instant::now()
}

/// Semantic-idle and absolute deadlines for one dispatched backend request.
struct RequestDeadlines {
    /// Non-renewable request dispatch time.
    dispatched_at: Instant,
    /// Last accepted semantic assistant output, initially request dispatch.
    semantic_progress_at: Instant,
    /// Last observed parser generation for accepted semantic assistant output.
    semantic_progress_generation: u64,
    /// Idle duration selected when the request was dispatched.
    idle_timeout: Duration,
    /// Absolute duration selected when the request was dispatched.
    absolute_timeout: Duration,
}

impl RequestDeadlines {
    /// Start both request clocks at backend dispatch.
    fn new(dispatched_at: Instant) -> Self {
        #[cfg(test)]
        let (idle_timeout, absolute_timeout) = TEST_REQUEST_DEADLINE_LIMITS
            .with(Cell::get)
            .unwrap_or((STREAM_IDLE_TIMEOUT, STREAM_ABSOLUTE_TIMEOUT));
        #[cfg(not(test))]
        let (idle_timeout, absolute_timeout) = (STREAM_IDLE_TIMEOUT, STREAM_ABSOLUTE_TIMEOUT);
        Self {
            dispatched_at,
            semantic_progress_at: dispatched_at,
            semantic_progress_generation: 0,
            idle_timeout,
            absolute_timeout,
        }
    }

    /// Renew the idle clock only after the parser accepts new semantic output.
    fn observe(&mut self, state: &StreamState) {
        if self.semantic_progress_generation != state.semantic_progress_generation {
            self.semantic_progress_generation = state.semantic_progress_generation;
            self.semantic_progress_at = state
                .semantic_progress_at
                .expect("a semantic generation always records its acceptance time");
        }
    }

    /// Return whether either request lifetime bound has expired.
    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.semantic_progress_at) >= self.idle_timeout
            || now.duration_since(self.dispatched_at) >= self.absolute_timeout
    }

    /// Bound one cancellation poll by the nearest request deadline.
    fn poll_timeout(&self, now: Instant) -> Duration {
        let idle_remaining = self
            .idle_timeout
            .saturating_sub(now.duration_since(self.semantic_progress_at));
        let absolute_remaining = self
            .absolute_timeout
            .saturating_sub(now.duration_since(self.dispatched_at));
        STREAM_READ_POLL_TIMEOUT
            .min(idle_remaining)
            .min(absolute_remaining)
    }
}

/// Install short request deadlines around one focused production-path test.
#[cfg(test)]
fn with_test_request_deadline_limits<R>(
    idle_timeout: Duration,
    absolute_timeout: Duration,
    now: Instant,
    run: impl FnOnce() -> R,
) -> R {
    TEST_REQUEST_DEADLINE_LIMITS.with(|limits| {
        assert!(
            limits
                .replace(Some((idle_timeout, absolute_timeout)))
                .is_none(),
            "test request deadline override must not nest"
        );
        TEST_REQUEST_NOW.with(|clock| {
            assert!(
                clock.replace(Some(now)).is_none(),
                "test request clock override must not nest"
            );
        });
        let result = run();
        TEST_REQUEST_NOW.with(|clock| clock.set(None));
        limits.set(None);
        result
    })
}

/// Advance the deterministic request clock inside one focused test callback.
#[cfg(test)]
fn set_test_request_now(now: Instant) {
    TEST_REQUEST_NOW.with(|clock| {
        assert!(clock.get().is_some(), "test request clock is not installed");
        clock.set(Some(now));
    });
}

/// Raw provider-event retention for one optional debug response capture.
enum DebugEventCapture {
    /// Debug capture is disabled, so provider events are never cloned or
    /// retained.
    Disabled,
    /// Debug capture is enabled and retains at most the established event
    /// limit.
    Enabled(Vec<serde_json::Value>),
}

impl DebugEventCapture {
    /// Start raw-event retention only when debug capture is enabled.
    fn new(enabled: bool) -> Self {
        if enabled {
            Self::Enabled(Vec::new())
        } else {
            Self::Disabled
        }
    }

    /// Retain one parsed event when capture is enabled and below its event
    /// limit.
    fn record(&mut self, event: &serde_json::Value) {
        let Self::Enabled(events) = self else {
            return;
        };
        if events.len() < MAX_DEBUG_EVENTS {
            events.push(event.clone());
        }
    }

    /// Borrow retained raw events, or an empty slice when capture is disabled.
    fn events(&self) -> &[serde_json::Value] {
        match self {
            Self::Disabled => &[],
            Self::Enabled(events) => events,
        }
    }
}

#[cfg(test)]
std::thread_local! {
    static OUTPUT_MATERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Resolved wire configuration for one finite Chat Completions attempt.
#[derive(Clone)]
pub struct AttemptConfig {
    /// Base URL without `/chat/completions`.
    pub base_url: String,
    /// Optional bearer token.
    pub api_key: String,
    /// Maximum requested output tokens, or zero to omit the cap.
    pub max_output_tokens: u32,
    /// Explicit Tau summary compactor limits, or `None` when unsupported.
    pub local_summary_compaction: Option<LocalSummaryCompactionConfig>,
    /// Non-standard, non-conflicting request members.
    pub extra_body: BTreeMap<String, serde_json::Value>,
    /// Optional OpenAI-compatible request fields supported by this route.
    pub compat: AttemptCompat,
}

/// Validated limits for Tau-owned summary compaction.
pub use tau_provider::local_summary_compaction::Config as LocalSummaryCompactionConfig;

/// Wire capabilities selected by the extension for one attempt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AttemptCompat {
    /// Send `stream_options.include_usage`.
    pub stream_options: bool,
    /// Send `parallel_tool_calls` when tools are present.
    pub parallel_tool_calls: bool,
    /// Send `tool_choice` when Tau requests automatic or disabled tool use.
    pub tool_choice: bool,
    /// Typed OpenAI prompt-cache controls explicitly selected for this route.
    pub prompt_cache: Option<PromptCache>,
    /// Send the selected reasoning effort with this route's spelling policy.
    pub reasoning_effort: Option<ReasoningEffortWire>,
    /// Assistant reasoning fields emitted during transcript replay.
    pub reasoning_replay: ReasoningReplay,
    /// Reject transcript roles that would create a second system message.
    pub single_initial_system_message: bool,
    /// Use `max_completion_tokens` rather than `max_tokens`.
    pub max_completion_tokens: bool,
    /// Explicit provider usage schema accepted from this route.
    pub cache_usage: CacheUsageCompat,
}

/// Provider-specific spelling for Tau's extended reasoning effort levels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningEffortWire {
    /// Collapse efforts above `high` to the OpenAI-compatible `high` spelling.
    OpenAi,
    /// Preserve extended Tau effort spellings, including literal `xhigh`.
    Literal,
}

/// Assistant reasoning fields emitted during semantic transcript replay.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReasoningReplay {
    /// Emit only `reasoning_content`.
    #[default]
    ReasoningContent,
    /// Emit only `reasoning`.
    Reasoning,
    /// Emit both aliases with identical text.
    Both,
}

/// Typed OpenAI prompt-cache controls selected for one Chat Completions route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptCache {
    /// Use OpenAI's legacy automatic caching with an explicit retention value.
    Legacy {
        /// Legacy retention sent as `prompt_cache_retention`.
        retention: PromptCacheRetention,
    },
    /// Mark the stable system prompt and disable implicit cache writes.
    ExplicitSystemPrompt,
}

/// Legacy OpenAI prompt-cache retention values supported by this adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptCacheRetention {
    /// Use the provider's ordinary in-memory retention behavior.
    InMemory,
    /// Request the provider's 24-hour retention behavior.
    Hours24,
}

impl PromptCacheRetention {
    /// Return the exact OpenAI wire spelling for this retention selection.
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::InMemory => "in_memory",
            Self::Hours24 => "24h",
        }
    }
}

/// Provider-specific cache usage wire schema selected by configuration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CacheUsageCompat {
    /// Ignore cache-specific usage fields.
    #[default]
    None,
    /// Parse OpenAI-compatible cached-read and cache-write counters.
    OpenAi,
    /// Parse DeepSeek-compatible cache hit and miss counters.
    DeepSeek,
}

/// Model wire identity for one attempt.
#[derive(Clone, Debug)]
pub struct AttemptModel {
    /// Upstream model id sent in the request.
    pub id: ModelName,
    /// Whether this exact route accepts native images in Function tool results.
    pub supports_image_tool_results: bool,
}

/// Capability and aggregate image limits for one complete provider request.
///
/// One value travels through every transcript block so capability authority and
/// both byte counters cannot diverge or reset at block boundaries.
struct ImageRequestBudget {
    /// Whether the exact resolved provider/model route accepts image tool
    /// results.
    supports_image_tool_results: bool,
    /// Aggregate canonical encoded image bytes admitted to this request.
    image_bytes: usize,
    /// Aggregate base64 data-URL bytes admitted to this request.
    data_url_bytes: usize,
}

impl ImageRequestBudget {
    /// Start one request budget with the exact resolved route capability.
    fn new(supports_image_tool_results: bool) -> Self {
        Self {
            supports_image_tool_results,
            image_bytes: 0,
            data_url_bytes: 0,
        }
    }

    /// Reserve raw and expanded capacity for one image without partial updates.
    fn reserve(&mut self, image: &tau_proto::ImageContent) -> bool {
        let encoded_len = image.data.len().div_ceil(3).saturating_mul(4);
        let data_url_len = "data:;base64,"
            .len()
            .saturating_add(image.media_type.mime_type().len())
            .saturating_add(encoded_len);
        let next_image_bytes = self.image_bytes.saturating_add(image.data.len());
        let next_data_url_bytes = self.data_url_bytes.saturating_add(data_url_len);
        if MAX_REQUEST_IMAGE_BYTES < next_image_bytes
            || MAX_REQUEST_IMAGE_DATA_URL_BYTES < next_data_url_bytes
        {
            return false;
        }
        self.image_bytes = next_image_bytes;
        self.data_url_bytes = next_data_url_bytes;
        true
    }
}

#[derive(Debug)]
enum LlmError {
    EmptyResponse,
    /// Redacted route- and phase-scoped shared network failure.
    Outbound(tau_provider::OutboundError),
    HttpStatus(u16, String),
    HttpStatusHinted(u16, String, Duration),
    Io(std::io::Error),
    Json(serde_json::Error),
    RepetitionDetected(tau_provider::StreamRepetition),
    Canceled,
    UnsupportedToolType(ToolType),
    UnsupportedMessageRole,
    UnknownFinishReason,
    ExtraBodyCollision(String),
    InvalidCompaction(String),
    PromptCacheSystemPromptRequired,
    StreamError(StreamFailure),
}

#[derive(Debug)]
struct StreamFailure {
    retry: Option<RetryDecision>,
    failure_kind: Option<tau_proto::ProviderFailureKind>,
}

/// Closed outcome for an HTTP status or structured provider error identifier.
enum ErrorClassification {
    /// The same prompt must not be retried.
    Terminal(tau_proto::ProviderFailureKind),
    /// The scheduler may retry according to the typed class.
    Retry(RetryClass),
}

impl ErrorClassification {
    fn from_http_error(status: u16, body: &str) -> Self {
        serde_json::from_str(body)
            .ok()
            .and_then(|value| {
                CanonicalIdentifierFamily::from_http_envelope(&value)
                    .classified()
                    .and_then(classify_structured_identifier)
            })
            .unwrap_or_else(|| Self::from_numeric_status(status))
    }

    fn from_numeric_status(status: u16) -> Self {
        match status {
            401 | 403 => Self::Retry(RetryClass::Auth),
            408 | 425 => Self::Retry(RetryClass::Transport),
            429 => Self::Retry(RetryClass::Throttle),
            400..=499 => Self::Terminal(tau_proto::ProviderFailureKind::RequestRejected),
            500..=599 => Self::Retry(RetryClass::Overload),
            _ => Self::Retry(RetryClass::Unknown),
        }
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyResponse => write!(f, "provider returned an empty response"),
            Self::Outbound(error) => error.fmt(f),
            Self::HttpStatus(code, _) | Self::HttpStatusHinted(code, _, _) => {
                write!(f, "provider returned HTTP {code}")
            }
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Json(error) => write!(f, "JSON error: {error}"),
            Self::RepetitionDetected(repetition) => write!(f, "{repetition}"),
            Self::Canceled => write!(f, "cancelled by harness"),
            Self::UnsupportedToolType(tool_type) => {
                write!(f, "Chat Completions does not support {tool_type:?} tools")
            }
            Self::UnsupportedMessageRole => write!(
                f,
                "provider chat template accepts only the initial system message"
            ),
            Self::UnknownFinishReason => {
                write!(f, "provider returned an unsupported finish reason")
            }
            Self::InvalidCompaction(message) => write!(f, "{message}"),
            Self::PromptCacheSystemPromptRequired => write!(
                f,
                "explicit OpenAI prompt caching requires a non-empty system prompt"
            ),
            Self::ExtraBodyCollision(field) => {
                write!(
                    f,
                    "extra_body conflicts with reserved request field `{field}`"
                )
            }
            Self::StreamError(_) => f.write_str("provider returned a streamed error"),
        }
    }
}

impl LlmError {
    fn retry_decision(&self) -> Option<RetryDecision> {
        match self {
            Self::RepetitionDetected(_)
            | Self::Canceled
            | Self::UnsupportedToolType(_)
            | Self::UnsupportedMessageRole
            | Self::UnknownFinishReason
            | Self::InvalidCompaction(_)
            | Self::PromptCacheSystemPromptRequired
            | Self::ExtraBodyCollision(_) => None,
            Self::StreamError(failure) => failure.retry.clone(),
            Self::Outbound(error) => Some(RetryDecision::new(outbound_retry_class(error.kind()))),
            Self::Io(_) => Some(RetryDecision::new(RetryClass::Transport)),
            Self::EmptyResponse | Self::Json(_) => Some(RetryDecision::new(RetryClass::Unknown)),
            Self::HttpStatus(code, body) => retry_decision_for_http_error(*code, body, None),
            Self::HttpStatusHinted(code, body, hint) => {
                retry_decision_for_http_error(*code, body, Some(*hint))
            }
        }
    }

    fn failure_kind(&self) -> Option<tau_proto::ProviderFailureKind> {
        match self {
            Self::HttpStatus(status, body) | Self::HttpStatusHinted(status, body, _) => {
                http_failure_kind(*status, body)
            }
            Self::ExtraBodyCollision(_)
            | Self::PromptCacheSystemPromptRequired
            | Self::InvalidCompaction(_) => Some(tau_proto::ProviderFailureKind::RequestRejected),
            Self::StreamError(failure) => failure.failure_kind,
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

/// Classifies provider-authored HTTP failures for retry cadence.
///
/// Exact accepted canonical identifiers override status, then the closed status
/// table decides whether retrying unchanged input is permitted.
fn retry_decision_for_http_error(
    status: u16,
    body: &str,
    header_hint: Option<Duration>,
) -> Option<RetryDecision> {
    let ErrorClassification::Retry(class) = ErrorClassification::from_http_error(status, body)
    else {
        return None;
    };
    let body_hint = parse_json_reset_hint(body, SystemTime::now());
    Some(RetryDecision::new(class).with_retry_after(header_hint.into_iter().chain(body_hint).max()))
}

fn http_failure_kind(status: u16, body: &str) -> Option<tau_proto::ProviderFailureKind> {
    match ErrorClassification::from_http_error(status, body) {
        ErrorClassification::Terminal(failure_kind) => Some(failure_kind),
        ErrorClassification::Retry(_) => None,
    }
}

fn classify_structured_identifier(identifier: &str) -> Option<ErrorClassification> {
    if identifier == "context_length_exceeded" {
        Some(ErrorClassification::Terminal(
            tau_proto::ProviderFailureKind::ContextWindowExceeded,
        ))
    } else {
        let class = classify_error_code(identifier);
        (class != RetryClass::Unknown).then_some(ErrorClassification::Retry(class))
    }
}

/// Successful parsed result of one finite backend attempt.
#[derive(Debug)]
pub struct AttemptSuccess {
    /// Attempt correlation and backend reachability.
    pub facts: AttemptFacts,
    /// Parsed semantic output items in provider order.
    pub output_items: Vec<ContextItem>,
    /// Provider stop reason.
    pub stop_reason: ProviderStopReason,
    /// Provider token usage, when reported.
    pub usage: Option<ProviderTokenUsage>,
    /// Cumulative transport/semantic response bytes at completion.
    pub response_bytes_received: u64,
    /// Stable backend output slots used by the terminal sampler flush.
    pub progress_items: Vec<AttemptOutputItem>,
}

/// One materialized semantic item with its stable backend output index.
#[derive(Clone, Debug)]
pub struct AttemptOutputItem {
    /// Backend-owned output index, including earlier incomplete slots.
    pub output_index: u32,
    /// Current semantic item value for this slot.
    pub item: ContextItem,
    /// Display replacement generation for cursor-based transient sampling.
    pub display_generation: DisplayGeneration,
}

/// Opaque identity for one append-compatible display generation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayGeneration(u64);

/// One borrowed display channel from accepted streaming state.
pub struct DisplayOutput<'a> {
    /// Stable provider output index.
    pub output_index: u32,
    /// Whether this channel is assistant text or full reasoning text.
    pub kind: DisplayOutputKind,
    /// Cumulative UTF-8 text accepted for this channel.
    pub text: &'a str,
    /// Replacement generation; Chat Completions channels are append-only.
    pub generation: DisplayGeneration,
}

/// Display channel kind exposed to the extension sampler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayOutputKind {
    /// Assistant narrative text.
    Message,
    /// Full reasoning text.
    Reasoning,
}

/// Typed terminal backend failure.
#[derive(Debug)]
pub struct AttemptFailure {
    /// Attempt correlation and backend reachability.
    pub facts: AttemptFacts,
    /// Bounded safe diagnostic suitable for the final provider event.
    pub message: String,
    /// Durable closed failure category, when known.
    pub failure_kind: Option<tau_proto::ProviderFailureKind>,
    /// Stop reason distinguishing repetition from other failures.
    pub stop_reason: ProviderStopReason,
    /// Whether semantic model output was parsed before failure.
    pub progress: SemanticProgress,
}

/// Semantic progress observed during a finite attempt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SemanticProgress {
    /// No model-authored output was parsed.
    #[default]
    None,
    /// Model-authored text, reasoning, or tool data was parsed.
    Parsed,
}

/// Correlation and reachability facts for one finite Chat attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptFacts {
    /// One-based scheduler attempt owning this finite operation.
    pub provider_attempt: tau_proto::ProviderAttempt,
    /// Number of actual request dispatches within the logical attempt.
    pub wire_dispatches: u64,
    /// Whether a request crossed the provider egress boundary.
    pub backend_reached: bool,
}

/// Typed lifecycle state owned by one finite Chat provider attempt.
struct ProviderAttemptContext {
    /// Operation whose policy consumes the normalized evidence.
    operation: tau_proto::PromptOperation,
    /// One-based scheduler attempt.
    provider_attempt: tau_proto::ProviderAttempt,
    /// Actual upstream request dispatches.
    wire_dispatches: Cell<u64>,
    /// Sticky parser-accepted semantic progress.
    progress: Cell<SemanticProgress>,
}

impl ProviderAttemptContext {
    /// Start one operation before provider egress.
    fn new(
        operation: tau_proto::PromptOperation,
        provider_attempt: tau_proto::ProviderAttempt,
    ) -> Self {
        Self {
            operation,
            provider_attempt,
            wire_dispatches: Cell::new(0),
            progress: Cell::new(SemanticProgress::None),
        }
    }

    /// Retain semantic progress monotonically.
    fn observe(&self, progress: SemanticProgress) {
        if progress == SemanticProgress::Parsed {
            self.progress.set(SemanticProgress::Parsed);
        }
    }

    /// Record one actual provider dispatch.
    fn dispatched(&self) {
        self.wire_dispatches
            .set(self.wire_dispatches.get().saturating_add(1));
    }

    /// Close immutable correlation and reachability facts.
    fn facts(&self) -> AttemptFacts {
        let wire_dispatches = self.wire_dispatches.get();
        AttemptFacts {
            provider_attempt: self.provider_attempt,
            wire_dispatches,
            backend_reached: wire_dispatches != 0,
        }
    }
}

/// Result of exactly one Chat Completions backend attempt.
#[derive(Debug)]
pub enum AttemptOutcome {
    /// The provider completed successfully.
    Completed(AttemptSuccess),
    /// The logical prompt remains pending for extension-owned scheduling.
    Retryable {
        /// Structured facts for extension-owned scheduling.
        decision: RetryDecision,
        /// Semantic progress parsed before the failure.
        progress: SemanticProgress,
        /// Attempt correlation and backend reachability.
        facts: AttemptFacts,
    },
    /// The harness canceled the active attempt.
    Canceled {
        /// Semantic progress parsed before cancellation.
        progress: SemanticProgress,
        /// Attempt correlation and backend reachability.
        facts: AttemptFacts,
    },
    /// The provider deterministically rejected the request or stream.
    Terminal(AttemptFailure),
}

/// Read-only accumulated progress exposed to the extension-owned sampler.
pub struct AttemptProgress<'a> {
    /// Prompt-local backend state borrowed only for the callback invocation.
    state: &'a StreamState,
}

impl AttemptProgress<'_> {
    /// Return the semantic output accumulated so far.
    #[must_use]
    pub fn materialize_output(&self) -> Vec<AttemptOutputItem> {
        self.state.indexed_output_items()
    }

    /// Visit cumulative display channels without cloning or parsing semantic
    /// output items.
    pub fn visit_display_output(&self, mut visit: impl FnMut(DisplayOutput<'_>)) {
        for (index, item) in self.state.output_items.iter().enumerate() {
            let (kind, text) = match item {
                OutputItemAccumulator::Message(text) => (DisplayOutputKind::Message, text),
                OutputItemAccumulator::Reasoning(text) => (DisplayOutputKind::Reasoning, text),
                OutputItemAccumulator::ToolCall(_) => continue,
            };
            if !text.is_empty() {
                visit(DisplayOutput {
                    output_index: index.try_into().unwrap_or(u32::MAX),
                    kind,
                    text,
                    generation: DisplayGeneration::default(),
                });
            }
        }
    }

    /// Return cumulative provider response bytes.
    #[must_use]
    pub fn response_bytes_received(&self) -> u64 {
        self.state.response_bytes_received()
    }

    /// Return whether any model-authored semantic bytes were parsed.
    #[must_use]
    pub fn semantic_progress(&self) -> SemanticProgress {
        self.state.semantic_progress
    }

    /// Return whether the accepted state contains output that qualifies for
    /// first-semantic-output timing.
    #[must_use]
    pub fn has_timed_semantic_output(&self) -> bool {
        self.state.has_timed_semantic_output()
    }
}

/// One synchronous observation from a finite Chat Completions attempt.
pub enum AttemptUpdate<'a> {
    /// The backend request is about to be sent for the first time.
    Dispatched(Instant),
    /// Accepted parser state changed or transport progress was observed.
    Progress(AttemptProgress<'a>),
}

/// Run exactly one finite provider attempt without event writes or retry
/// sleeps.
#[allow(clippy::too_many_arguments)]
pub fn run_attempt(
    prompt: &tau_proto::AgentPromptCreated,
    config: &AttemptConfig,
    model: &AttemptModel,
    debug_provider_requests: bool,
    on_update: &mut impl FnMut(AttemptUpdate<'_>),
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
) -> AttemptOutcome {
    run_attempt_numbered(
        tau_proto::ProviderAttempt::ONE,
        prompt,
        config,
        model,
        debug_provider_requests,
        on_update,
        is_canceled,
        network,
    )
}

/// Run one finite provider attempt with scheduler-owned correlation.
#[allow(clippy::too_many_arguments)]
pub fn run_attempt_numbered(
    provider_attempt: tau_proto::ProviderAttempt,
    prompt: &tau_proto::AgentPromptCreated,
    config: &AttemptConfig,
    model: &AttemptModel,
    debug_provider_requests: bool,
    on_update: &mut impl FnMut(AttemptUpdate<'_>),
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
) -> AttemptOutcome {
    let mut private_trace = private_trace::AttemptTrace::selected(
        private_trace::Backend::ChatCompletions,
        private_trace::Transport::HttpSse,
    );
    let attempt = ProviderAttemptContext::new(prompt.operation, provider_attempt);
    debug_assert_eq!(attempt.operation, prompt.operation);
    let result = {
        let on_attempt_update = path_std_cell::RefCell::new(on_update);
        let mut on_state_update = |state: &StreamState| {
            let snapshot = AttemptProgress { state };
            attempt.observe(snapshot.semantic_progress());
            on_attempt_update.borrow_mut()(AttemptUpdate::Progress(snapshot));
        };
        let mut on_dispatched = |at| {
            attempt.dispatched();
            on_attempt_update.borrow_mut()(AttemptUpdate::Dispatched(at));
        };
        chat_completions_stream(
            provider_attempt,
            config,
            model,
            prompt,
            debug_provider_requests,
            &mut on_state_update,
            &mut on_dispatched,
            is_canceled,
            network,
            &mut private_trace,
        )
    };
    if let Some(trace) = private_trace.take() {
        let outcome = match &result {
            Ok(_) => private_trace::Outcome::Completed,
            Err(LlmError::Canceled) => private_trace::Outcome::Canceled,
            Err(error) if error.retry_decision().is_some() => private_trace::Outcome::Retryable,
            Err(_) => private_trace::Outcome::Failed,
        };
        trace.finish(outcome);
    }
    finish_attempt_with_facts(result, attempt.progress.get(), attempt.facts())
}

#[cfg(test)]
fn finish_attempt(
    result: Result<StreamState, LlmError>,
    progress: SemanticProgress,
) -> AttemptOutcome {
    finish_attempt_with_facts(
        result,
        progress,
        AttemptFacts {
            provider_attempt: tau_proto::ProviderAttempt::ONE,
            wire_dispatches: 0,
            backend_reached: false,
        },
    )
}

fn finish_attempt_with_facts(
    result: Result<StreamState, LlmError>,
    progress: SemanticProgress,
    facts: AttemptFacts,
) -> AttemptOutcome {
    match result {
        Ok(state) => AttemptOutcome::Completed(AttemptSuccess {
            facts,
            progress_items: state.indexed_output_items(),
            output_items: state.output_items(),
            stop_reason: state.stop_reason,
            usage: state.usage(),
            response_bytes_received: state.response_bytes_received(),
        }),
        Err(LlmError::Canceled) => AttemptOutcome::Canceled { progress, facts },
        Err(error) => match error.retry_decision() {
            Some(decision) => AttemptOutcome::Retryable {
                decision,
                progress,
                facts,
            },
            None => AttemptOutcome::Terminal(AttemptFailure {
                facts,
                message: bounded_provider_error(&format!("LLM error: {error}")),
                failure_kind: error.failure_kind(),
                stop_reason: if matches!(error, LlmError::RepetitionDetected(_)) {
                    ProviderStopReason::RepetitionDetected
                } else {
                    ProviderStopReason::Error
                },
                progress,
            }),
        },
    }
}

struct StreamState {
    text: String,
    thinking: String,
    output_items: Vec<OutputItemAccumulator>,
    pending_content: String,
    in_think_tag: bool,
    tool_call_output_indices: HashMap<usize, usize>,
    input_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    cache_miss_tokens: Option<u64>,
    cache_usage: CacheUsageCompat,
    output_tokens: Option<u64>,
    stop_reason: ProviderStopReason,
    repetition_guard: StreamRepetitionGuard,
    transport_response_bytes: u64,
    semantic_progress: SemanticProgress,
    /// Generation renewed by newly accepted assistant text, reasoning, or tool
    /// name/argument output, but not identifiers or transport-only events.
    semantic_progress_generation: u64,
    /// Exact synchronous acceptance time for the latest semantic generation.
    semantic_progress_at: Option<Instant>,
    /// Compact-only event validator, absent during ordinary inference.
    compact_validator: Option<compact_stream::CompactStreamValidator>,
}

impl StreamState {
    #[cfg(test)]
    fn new() -> Self {
        Self::new_with_cache_usage(CacheUsageCompat::None)
    }

    /// Construct empty parser state for an explicitly enabled cache schema.
    #[cfg(test)]
    fn new_with_cache_usage(cache_usage: CacheUsageCompat) -> Self {
        Self::new_for_attempt(cache_usage, None)
    }

    /// Construct parser state with an optional compact-only output validator.
    fn new_for_attempt(
        cache_usage: CacheUsageCompat,
        compact_output_bytes: Option<tau_proto::ByteCount>,
    ) -> Self {
        Self {
            text: String::new(),
            thinking: String::new(),
            output_items: Vec::new(),
            pending_content: String::new(),
            in_think_tag: false,
            tool_call_output_indices: HashMap::new(),
            input_tokens: None,
            cached_tokens: None,
            cache_write_tokens: None,
            cache_miss_tokens: None,
            cache_usage,
            output_tokens: None,
            stop_reason: ProviderStopReason::EndTurn,
            repetition_guard: StreamRepetitionGuard::new(),
            transport_response_bytes: 0,
            semantic_progress: SemanticProgress::None,
            semantic_progress_generation: 0,
            semantic_progress_at: None,
            compact_validator: compact_output_bytes
                .map(compact_stream::CompactStreamValidator::new),
        }
    }

    /// Validate a standalone compact response before releasing its parsed
    /// items.
    fn validate_compaction(mut self) -> Result<Self, LlmError> {
        if let Some(validator) = self.compact_validator.take() {
            validator.finish(&self)?;
        }
        Ok(self)
    }

    fn output_items(&self) -> Vec<ContextItem> {
        self.output_items
            .iter()
            .filter_map(OutputItemAccumulator::context_item)
            .collect()
    }

    fn indexed_output_items(&self) -> Vec<AttemptOutputItem> {
        #[cfg(test)]
        OUTPUT_MATERIALIZATIONS.with(|count| count.set(count.get() + 1));
        self.output_items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                Some(AttemptOutputItem {
                    output_index: index.try_into().unwrap_or(u32::MAX),
                    item: item.context_item()?,
                    display_generation: DisplayGeneration::default(),
                })
            })
            .collect()
    }

    fn non_visible_output_bytes(&self) -> u64 {
        self.output_items
            .iter()
            .filter_map(|item| match item {
                OutputItemAccumulator::ToolCall(call) => Some(call.arguments.len() as u64),
                _ => None,
            })
            .sum()
    }

    fn response_bytes_received(&self) -> u64 {
        let visible_bytes = self
            .text
            .len()
            .saturating_add(self.thinking.len())
            .try_into()
            .unwrap_or(u64::MAX);
        let non_visible_bytes = self.non_visible_output_bytes();
        visible_bytes
            .saturating_add(non_visible_bytes)
            .max(self.transport_response_bytes)
    }

    fn has_timed_semantic_output(&self) -> bool {
        !self.text.is_empty()
            || !self.thinking.is_empty()
            || self.output_items.iter().any(|item| match item {
                OutputItemAccumulator::ToolCall(call) => {
                    !call.name.is_empty() || !call.arguments.is_empty()
                }
                OutputItemAccumulator::Message(text) | OutputItemAccumulator::Reasoning(text) => {
                    !text.is_empty()
                }
            })
    }

    fn record_transport_response_bytes(&mut self, bytes: usize) {
        self.transport_response_bytes = self
            .transport_response_bytes
            .saturating_add(bytes.try_into().unwrap_or(u64::MAX));
    }

    fn append_assistant_text_delta(&mut self, delta: &str) -> Result<(), LlmError> {
        if delta.is_empty() {
            return Ok(());
        }
        if let Some(validator) = &self.compact_validator {
            validator.check_append(self.text.len(), delta.len())?;
        }
        self.semantic_progress = SemanticProgress::Parsed;
        let output_index = match self.output_items.last() {
            Some(OutputItemAccumulator::Message(_)) => self.output_items.len() - 1,
            _ => self.output_items.len(),
        };
        if let Some(repetition) = self
            .repetition_guard
            .push_delta(StreamRepetitionKey::AssistantText { output_index }, delta)
        {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        self.text.push_str(delta);
        if let Some(OutputItemAccumulator::Message(text)) = self.output_items.last_mut() {
            text.push_str(delta);
        } else {
            self.output_items
                .push(OutputItemAccumulator::Message(delta.to_owned()));
        }
        self.record_deadline_semantic_progress();
        Ok(())
    }

    fn append_reasoning_delta(&mut self, delta: &str) -> Result<(), LlmError> {
        if delta.is_empty() {
            return Ok(());
        }
        if let Some(validator) = &self.compact_validator {
            validator.check_append(self.thinking.len(), delta.len())?;
        }
        self.semantic_progress = SemanticProgress::Parsed;
        let output_index = match self.output_items.last() {
            Some(OutputItemAccumulator::Reasoning(_)) => self.output_items.len() - 1,
            _ => self.output_items.len(),
        };
        if let Some(repetition) = self
            .repetition_guard
            .push_delta(StreamRepetitionKey::ReasoningText { output_index }, delta)
        {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        self.thinking.push_str(delta);
        if let Some(OutputItemAccumulator::Reasoning(reasoning)) = self.output_items.last_mut() {
            reasoning.push_str(delta);
        } else {
            self.output_items
                .push(OutputItemAccumulator::Reasoning(delta.to_owned()));
        }
        self.record_deadline_semantic_progress();
        Ok(())
    }

    fn append_tool_arguments_delta(
        &mut self,
        stream_index: usize,
        delta: &str,
    ) -> Result<(), LlmError> {
        if !delta.is_empty() {
            self.semantic_progress = SemanticProgress::Parsed;
        }
        let output_index = *self
            .tool_call_output_indices
            .get(&stream_index)
            .unwrap_or(&self.output_items.len());
        if let Some(repetition) = self.repetition_guard.push_delta(
            StreamRepetitionKey::FunctionCallArguments { output_index },
            delta,
        ) {
            return Err(LlmError::RepetitionDetected(repetition));
        }
        self.tool_call_at_mut(stream_index)
            .arguments
            .push_str(delta);
        if !delta.is_empty() {
            self.record_deadline_semantic_progress();
        }
        Ok(())
    }

    /// Record exact semantic acceptance before any caller-controlled callback.
    fn record_deadline_semantic_progress(&mut self) {
        self.semantic_progress_generation = self.semantic_progress_generation.saturating_add(1);
        self.semantic_progress_at = Some(request_now());
    }

    fn tool_call_at_mut(&mut self, stream_index: usize) -> &mut ToolCallAccumulator {
        let output_index = *self
            .tool_call_output_indices
            .entry(stream_index)
            .or_insert_with(|| {
                let output_index = self.output_items.len();
                self.output_items.push(OutputItemAccumulator::ToolCall(
                    ToolCallAccumulator::default(),
                ));
                output_index
            });
        let OutputItemAccumulator::ToolCall(call) = &mut self.output_items[output_index] else {
            unreachable!("tool-call slot was just initialized");
        };
        call
    }

    fn usage(&self) -> Option<ProviderTokenUsage> {
        if self.input_tokens.is_none()
            && self.cached_tokens.is_none()
            && self.output_tokens.is_none()
        {
            return None;
        }
        let input = self.input_tokens.unwrap_or(0);
        let cached = self.cached_tokens.unwrap_or(0).min(input);
        let output = self.output_tokens.unwrap_or(0);
        let cache = normalize_cache_usage(
            self.cache_usage,
            input,
            self.cached_tokens,
            self.cache_write_tokens,
            self.cache_miss_tokens,
        );
        Some(ProviderTokenUsage {
            model: None,
            prompt_sent_tokens: input,
            prompt_cached_tokens: cached,
            prompt_cache_read_ceiling_tokens: None,
            cache,
            response_received_tokens: output,
            stats: Default::default(),
        })
    }

    fn has_output_items(&self) -> bool {
        self.output_items.iter().any(|item| match item {
            OutputItemAccumulator::Message(text) => !text.is_empty(),
            OutputItemAccumulator::Reasoning(reasoning) => !reasoning.is_empty(),
            OutputItemAccumulator::ToolCall(call) => !call.name.is_empty(),
        })
    }

    fn is_empty_end_turn(&self) -> bool {
        self.stop_reason == ProviderStopReason::EndTurn && !self.has_output_items()
    }
}

enum OutputItemAccumulator {
    Message(String),
    Reasoning(String),
    ToolCall(ToolCallAccumulator),
}

impl OutputItemAccumulator {
    fn context_item(&self) -> Option<ContextItem> {
        match self {
            Self::Message(text) => (!text.is_empty()).then(|| assistant_text_item(text.clone())),
            Self::Reasoning(reasoning) => reasoning_text_context_item(reasoning),
            Self::ToolCall(call) => call.context_item(),
        }
    }
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    fn context_item(&self) -> Option<ContextItem> {
        if self.name.is_empty() {
            return None;
        }
        Some(ContextItem::ToolCall(ToolCallItem {
            call_id: self.id.clone().into(),
            name: tau_proto::ToolName::new(self.name.clone()),
            tool_type: ToolType::Function,
            arguments: serde_json::from_str::<serde_json::Value>(&self.arguments)
                .map(|value| json_to_cbor(&value))
                .unwrap_or(tau_proto::CborValue::Null),
            raw_arguments_json: Some(self.arguments.clone()),
            responses_envelope: None,
        }))
    }
}

#[cfg(test)]
fn read_chat_stream_body(
    mut reader: impl Read,
    state: &mut StreamState,
    raw_events: &mut DebugEventCapture,
    on_update: &mut impl FnMut(&StreamState),
    is_canceled: &mut impl FnMut() -> bool,
) -> Result<(), LlmError> {
    let mut buffer = [0; 8192];
    let mut pending = Vec::new();
    let mut deadlines = RequestDeadlines::new(request_now());
    loop {
        if is_canceled() {
            return Err(LlmError::Canceled);
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                apply_pending_sse_line(&mut pending, state, raw_events, on_update)?;
                return Ok(());
            }
            Ok(bytes) => {
                let outcome = process_stream_chunk(
                    &buffer[..bytes],
                    &mut pending,
                    state,
                    raw_events,
                    on_update,
                )?;
                deadlines.observe(state);
                if outcome.done {
                    return Ok(());
                }
                if deadlines.expired(request_now()) {
                    return Err(LlmError::Io(path_std_io::Error::new(
                        path_std_io::ErrorKind::TimedOut,
                        "provider stream deadline exceeded",
                    )));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                if deadlines.expired(request_now()) {
                    return Err(LlmError::Io(path_std_io::Error::new(
                        path_std_io::ErrorKind::TimedOut,
                        "provider stream deadline exceeded",
                    )));
                }
            }
            Err(error) => return Err(LlmError::Io(error)),
        }
    }
}

/// Terminal-marker and provider-data observations from parsed SSE chunk bytes.
struct SseChunkOutcome {
    /// Whether the current chunk carries the terminal SSE marker.
    done: bool,
    /// Whether the current chunk contains a provider `data:` field.
    #[cfg_attr(not(test), allow(dead_code))]
    provider_event: bool,
}

fn process_stream_chunk(
    bytes: &[u8],
    pending: &mut Vec<u8>,
    state: &mut StreamState,
    raw_events: &mut DebugEventCapture,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<SseChunkOutcome, LlmError> {
    state.record_transport_response_bytes(bytes.len());
    if state.transport_response_bytes > MAX_RESPONSE_BYTES {
        return Err(LlmError::Io(path_std_io::Error::new(
            path_std_io::ErrorKind::InvalidData,
            "provider response exceeds byte limit",
        )));
    }
    pending.extend_from_slice(bytes);
    let lines = take_complete_sse_lines(pending)?;
    let outcome = apply_chat_stream_lines(&lines, state, raw_events, on_update)?;
    on_update(state);
    Ok(outcome)
}

fn take_complete_sse_lines(pending: &mut Vec<u8>) -> Result<Vec<u8>, LlmError> {
    let complete_len = pending
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let has_oversized_complete_line = pending[..complete_len]
        .split_inclusive(|byte| *byte == b'\n')
        .any(|line| {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            MAX_SSE_LINE_BYTES < line.len()
        });
    if has_oversized_complete_line
        || MAX_SSE_LINE_BYTES < pending.len().saturating_sub(complete_len)
    {
        return Err(LlmError::Io(path_std_io::Error::new(
            path_std_io::ErrorKind::InvalidData,
            "provider SSE line exceeds limit",
        )));
    }
    if complete_len == 0 {
        return Ok(Vec::new());
    }
    let mut lines = std::mem::take(pending);
    *pending = lines.split_off(complete_len);
    Ok(lines)
}

fn sse_line_data(line: &[u8]) -> Option<&[u8]> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    line.strip_prefix(b"data: ")
}

fn apply_pending_sse_line(
    pending: &mut Vec<u8>,
    state: &mut StreamState,
    raw_events: &mut DebugEventCapture,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(), LlmError> {
    if pending.is_empty() {
        return Ok(());
    }
    let line = std::mem::take(pending);
    let _ = apply_chat_stream_lines(&line, state, raw_events, on_update)?;
    Ok(())
}

fn apply_chat_stream_lines(
    lines: &[u8],
    state: &mut StreamState,
    raw_events: &mut DebugEventCapture,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<SseChunkOutcome, LlmError> {
    let mut provider_event = false;
    for line in lines.split_inclusive(|byte| *byte == b'\n') {
        let Some(data) = sse_line_data(line) else {
            continue;
        };
        provider_event = true;
        if data == b"[DONE]" {
            return Ok(SseChunkOutcome {
                done: true,
                provider_event,
            });
        }
        let data = String::from_utf8_lossy(data);
        let event: serde_json::Value =
            serde_json::from_str(data.as_ref()).map_err(LlmError::Json)?;
        raw_events.record(&event);
        apply_event(state, &event, on_update)?;
    }
    Ok(SseChunkOutcome {
        done: false,
        provider_event,
    })
}

#[allow(clippy::too_many_arguments)] // Dispatch and state callbacks have distinct timing ownership.
fn chat_completions_stream(
    provider_attempt: tau_proto::ProviderAttempt,
    provider: &AttemptConfig,
    model: &AttemptModel,
    prompt: &tau_proto::AgentPromptCreated,
    debug_provider_requests: bool,
    on_update: &mut impl FnMut(&StreamState),
    on_dispatched: &mut impl FnMut(Instant),
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
    private_trace: &mut Option<private_trace::AttemptTrace>,
) -> Result<StreamState, LlmError> {
    let debug_provider_requests = debug_capture_enabled_for_prompt(prompt, debug_provider_requests);
    if is_canceled() {
        return Err(LlmError::Canceled);
    }
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let body = try_build_request(provider, model, prompt)?;
    if let Some(trace) = private_trace.as_mut() {
        trace.lowering_finished();
    }
    let serialization_started = private_trace::started(private_trace);
    let body_str = serde_json::to_string(&body).map_err(LlmError::Json)?;
    if let (Some(trace), Some(started)) = (private_trace.as_mut(), serialization_started) {
        trace.serialization_finished(started, body_str.len());
    }
    let runtime = path_tokio_runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(LlmError::Io)?;
    let mut on_wire_dispatch = |at| {
        // The standalone compactor transaction deliberately persists only its
        // accepted checkpoint. Debug capture remains private observability.
        maybe_debug_submit_provider_request(
            prompt,
            model,
            debug_provider_requests,
            &body,
            provider_attempt,
            1,
        );
        on_dispatched(at);
    };
    let result = runtime.block_on(chat_completions_stream_async(
        AsyncAttemptContext {
            url: &url,
            provider,
            body: body_str,
            prompt,
            capture_raw_events: debug_provider_requests,
        },
        on_update,
        &mut on_wire_dispatch,
        is_canceled,
        network,
        private_trace,
    ));
    let (mut state, raw_events) = match result {
        Ok(success) => success,
        Err(error) => {
            finalize_provider_capture(
                prompt,
                model,
                debug_provider_requests,
                provider_attempt,
                None,
                &error,
            );
            return Err(error);
        }
    };
    flush_pending_content(&mut state, on_update)?;
    maybe_debug_submit_provider_response(
        prompt,
        model,
        debug_provider_requests,
        &state,
        raw_events.events(),
        provider_attempt,
        1,
    );
    ensure_non_empty_end_turn(state.validate_compaction()?)
}

fn debug_capture_enabled_for_prompt(
    _prompt: &tau_proto::AgentPromptCreated,
    configured: bool,
) -> bool {
    configured
}

struct AsyncAttemptContext<'a> {
    /// Fully resolved Chat Completions endpoint.
    url: &'a str,
    /// Mutable-profile values resolved for this attempt.
    provider: &'a AttemptConfig,
    /// Serialized request body transferred into reqwest without another copy.
    body: String,
    /// Logical prompt used for diagnostics.
    prompt: &'a tau_proto::AgentPromptCreated,
    /// Whether this attempt should retain raw events for private debug capture.
    capture_raw_events: bool,
}

/// Recheck cancellation before classifying a dispatched request deadline.
fn ensure_request_active(
    deadlines: &RequestDeadlines,
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
    url: &str,
    phase: tau_provider::OutboundPhase,
) -> Result<(), LlmError> {
    if is_canceled() {
        return Err(LlmError::Canceled);
    }
    if deadlines.expired(request_now()) {
        return Err(LlmError::Outbound(network.deadline_error(url, phase)));
    }
    Ok(())
}

async fn chat_completions_stream_async(
    context: AsyncAttemptContext<'_>,
    on_update: &mut impl FnMut(&StreamState),
    on_dispatched: &mut impl FnMut(Instant),
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
    private_trace: &mut Option<private_trace::AttemptTrace>,
) -> Result<(StreamState, DebugEventCapture), LlmError> {
    let client = network
        .client_for(context.url)
        .map_err(LlmError::Outbound)?;
    let mut request = client
        .post(context.url)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(context.body);
    if !context.provider.api_key.trim().is_empty() {
        request = request.bearer_auth(&context.provider.api_key);
    }
    let mut send = Box::pin(request.send());
    let mut request_deadlines = None;
    let mut response = loop {
        let deadlines = request_deadlines.get_or_insert_with(|| {
            let at = request_now();
            if let Some(trace) = private_trace.as_mut() {
                trace.record_dispatch();
            }
            on_dispatched(at);
            RequestDeadlines::new(at)
        });
        ensure_request_active(
            deadlines,
            is_canceled,
            network,
            context.url,
            tau_provider::OutboundPhase::Request,
        )?;
        let now = request_now();
        if let Ok(result) = tokio::time::timeout(deadlines.poll_timeout(now), &mut send).await {
            ensure_request_active(
                deadlines,
                is_canceled,
                network,
                context.url,
                tau_provider::OutboundPhase::Request,
            )?;
            break result.map_err(|error| {
                LlmError::Outbound(network.reqwest_error(
                    context.url,
                    tau_provider::OutboundPhase::Request,
                    &error,
                ))
            })?;
        }
    };
    let mut request_deadlines =
        request_deadlines.expect("request dispatch precedes receiving response headers");
    if !response.status().is_success() {
        let code = response.status().as_u16();
        if let Some(error) = network.proxy_response_error(context.url, code) {
            return Err(LlmError::Outbound(error));
        }
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, SystemTime::now()));
        let mut bytes = Vec::new();
        while bytes.len() < MAX_HTTP_ERROR_BODY_BYTES as usize {
            ensure_request_active(
                &request_deadlines,
                is_canceled,
                network,
                context.url,
                tau_provider::OutboundPhase::Body,
            )?;
            let now = request_now();
            let polled =
                tokio::time::timeout(request_deadlines.poll_timeout(now), response.chunk()).await;
            ensure_request_active(
                &request_deadlines,
                is_canceled,
                network,
                context.url,
                tau_provider::OutboundPhase::Body,
            )?;
            match polled {
                Ok(Ok(Some(chunk))) => {
                    let remaining = MAX_HTTP_ERROR_BODY_BYTES as usize - bytes.len();
                    bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }
                Ok(Ok(None)) => break,
                Ok(Err(error)) => {
                    return Err(LlmError::Outbound(network.reqwest_error(
                        context.url,
                        tau_provider::OutboundPhase::Body,
                        &error,
                    )));
                }
                Err(_) => {}
            }
        }
        ensure_request_active(
            &request_deadlines,
            is_canceled,
            network,
            context.url,
            tau_provider::OutboundPhase::Body,
        )?;
        let body = String::from_utf8_lossy(&bytes).into_owned();
        return Err(match retry_after {
            Some(delay) => LlmError::HttpStatusHinted(code, body, delay),
            None => LlmError::HttpStatus(code, body),
        });
    }
    let compact_output_bytes =
        (context.prompt.operation == tau_proto::PromptOperation::StandaloneCompaction).then(|| {
            let max_output_bytes = context
                .provider
                .local_summary_compaction
                .expect("standalone request lowering already validated its compaction config")
                .max_output_bytes();
            tau_proto::ByteCount::new(max_output_bytes)
        });
    let mut state =
        StreamState::new_for_attempt(context.provider.compat.cache_usage, compact_output_bytes);
    let mut raw_events = DebugEventCapture::new(context.capture_raw_events);
    let mut pending = Vec::new();
    loop {
        ensure_request_active(
            &request_deadlines,
            is_canceled,
            network,
            context.url,
            tau_provider::OutboundPhase::Body,
        )?;
        let now = request_now();
        let polled =
            tokio::time::timeout(request_deadlines.poll_timeout(now), response.chunk()).await;
        ensure_request_active(
            &request_deadlines,
            is_canceled,
            network,
            context.url,
            tau_provider::OutboundPhase::Body,
        )?;
        match polled {
            Ok(Ok(Some(chunk))) => {
                if let Some(trace) = private_trace.as_mut() {
                    trace.first_input(chunk.len());
                }
                let mut boundary_error = None;
                let decode_started = private_trace::started(private_trace);
                let mut callback_elapsed = private_trace.as_ref().map(|_| Duration::ZERO);
                let parsed = {
                    let mut on_parser_update = |state: &StreamState| {
                        if boundary_error.is_some() {
                            return;
                        }
                        if state.has_timed_semantic_output()
                            && let Some(trace) = private_trace.as_mut()
                        {
                            trace.semantic_qualified();
                        }
                        let callback_started = private_trace::started(private_trace);
                        on_update(state);
                        if let (Some(elapsed), Some(started)) =
                            (callback_elapsed.as_mut(), callback_started)
                        {
                            *elapsed = elapsed.saturating_add(started.elapsed());
                        }
                        request_deadlines.observe(state);
                        boundary_error = ensure_request_active(
                            &request_deadlines,
                            is_canceled,
                            network,
                            context.url,
                            tau_provider::OutboundPhase::Body,
                        )
                        .err();
                    };
                    process_stream_chunk(
                        &chunk,
                        &mut pending,
                        &mut state,
                        &mut raw_events,
                        &mut on_parser_update,
                    )
                };
                if let (Some(trace), Some(started)) = (private_trace.as_mut(), decode_started) {
                    trace.decoded_excluding(started, callback_elapsed.unwrap_or_default(), false);
                }
                if let Some(error) = boundary_error {
                    return Err(error);
                }
                request_deadlines.observe(&state);
                ensure_request_active(
                    &request_deadlines,
                    is_canceled,
                    network,
                    context.url,
                    tau_provider::OutboundPhase::Body,
                )?;
                let outcome = parsed?;
                if outcome.done {
                    return Ok((state, raw_events));
                }
            }
            Ok(Ok(None)) => {
                let mut boundary_error = None;
                let decode_started = private_trace::started(private_trace);
                let mut callback_elapsed = private_trace.as_ref().map(|_| Duration::ZERO);
                let parsed = {
                    let mut on_parser_update = |state: &StreamState| {
                        if boundary_error.is_some() {
                            return;
                        }
                        if state.has_timed_semantic_output()
                            && let Some(trace) = private_trace.as_mut()
                        {
                            trace.semantic_qualified();
                        }
                        let callback_started = private_trace::started(private_trace);
                        on_update(state);
                        if let (Some(elapsed), Some(started)) =
                            (callback_elapsed.as_mut(), callback_started)
                        {
                            *elapsed = elapsed.saturating_add(started.elapsed());
                        }
                        request_deadlines.observe(state);
                        boundary_error = ensure_request_active(
                            &request_deadlines,
                            is_canceled,
                            network,
                            context.url,
                            tau_provider::OutboundPhase::Body,
                        )
                        .err();
                    };
                    apply_pending_sse_line(
                        &mut pending,
                        &mut state,
                        &mut raw_events,
                        &mut on_parser_update,
                    )
                };
                if let (Some(trace), Some(started)) = (private_trace.as_mut(), decode_started) {
                    trace.decoded_excluding(started, callback_elapsed.unwrap_or_default(), false);
                }
                if let Some(error) = boundary_error {
                    return Err(error);
                }
                request_deadlines.observe(&state);
                ensure_request_active(
                    &request_deadlines,
                    is_canceled,
                    network,
                    context.url,
                    tau_provider::OutboundPhase::Body,
                )?;
                parsed?;
                return Ok((state, raw_events));
            }
            Ok(Err(error)) => {
                return Err(LlmError::Outbound(network.reqwest_error(
                    context.url,
                    tau_provider::OutboundPhase::Body,
                    &error,
                )));
            }
            Err(_) => {}
        }
    }
}

fn ensure_non_empty_end_turn(state: StreamState) -> Result<StreamState, LlmError> {
    if state.is_empty_end_turn() {
        Err(LlmError::EmptyResponse)
    } else {
        Ok(state)
    }
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    /// Legacy OpenAI retention selected for an explicitly cache-capable route.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<&'static str>,
    /// Explicit OpenAI cache options selected for the stable system boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_options: Option<PromptCacheOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(flatten)]
    extra_body: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Explicit OpenAI cache options emitted for the stable system-prompt boundary.
#[derive(Serialize)]
struct PromptCacheOptions {
    /// Disable provider-created implicit breakpoints.
    mode: &'static str,
    /// Current OpenAI minimum lifetime for explicit cache entries.
    ttl: &'static str,
}

fn try_build_request(
    provider: &AttemptConfig,
    model: &AttemptModel,
    prompt: &tau_proto::AgentPromptCreated,
) -> Result<ChatRequest, LlmError> {
    validate_extra_body(&provider.extra_body)?;
    let summary_config = (prompt.operation == tau_proto::PromptOperation::StandaloneCompaction)
        .then(|| {
            provider.local_summary_compaction.ok_or_else(|| {
                LlmError::InvalidCompaction(
                    "standalone compaction is not enabled for this Chat Completions model"
                        .to_owned(),
                )
            })
        })
        .transpose()?;
    let explicit_system_prompt = matches!(
        provider.compat.prompt_cache,
        Some(PromptCache::ExplicitSystemPrompt)
    );
    if explicit_system_prompt && prompt.system_prompt.trim().is_empty() {
        return Err(LlmError::PromptCacheSystemPromptRequired);
    }
    with_admitted_historical_prefix(
        summary_config,
        &prompt.context,
        tau_provider::local_summary_compaction::historical_prefix_fits_json_budget,
        || {
            build_request_after_prefix_admission(
                provider,
                model,
                prompt,
                summary_config,
                explicit_system_prompt,
            )
        },
    )
}

fn build_request_after_prefix_admission(
    provider: &AttemptConfig,
    model: &AttemptModel,
    prompt: &tau_proto::AgentPromptCreated,
    summary_config: Option<LocalSummaryCompactionConfig>,
    explicit_system_prompt: bool,
) -> Result<ChatRequest, LlmError> {
    let mut context = prompt.context.clone();
    if summary_config.is_some() {
        tau_provider::local_summary_compaction::replace_trailing_trigger(&mut context)
            .map_err(|error| LlmError::InvalidCompaction(error.to_owned()))?;
    }
    let mut messages = Vec::new();
    if !prompt.system_prompt.trim().is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": if explicit_system_prompt {
                serde_json::json!([{
                    "type": "text",
                    "text": prompt.system_prompt,
                    "prompt_cache_breakpoint": {"mode": "explicit"},
                }])
            } else {
                serde_json::json!(prompt.system_prompt)
            },
        }));
    }
    if provider.compat.single_initial_system_message
        && prompt
            .context
            .blocks
            .iter()
            .any(context_block_has_system_authority)
    {
        return Err(LlmError::UnsupportedMessageRole);
    }
    let mut image_budget = ImageRequestBudget::new(model.supports_image_tool_results);
    for block in &context.blocks {
        append_context_block(
            block,
            provider.compat.reasoning_replay,
            &mut image_budget,
            &mut messages,
        );
    }
    let mut tools = prompt
        .tools
        .iter()
        .map(convert_tool_definition)
        .collect::<Result<Vec<_>, _>>()?;
    let tool_choice = match (
        provider.compat.tool_choice,
        prompt.tool_choice,
        tools.is_empty(),
    ) {
        (true, ToolChoice::None, _) => Some("none"),
        (true, ToolChoice::Auto, false) => Some("auto"),
        (true, ToolChoice::Auto, true) | (false, ToolChoice::Auto, _) => None,
        (false, ToolChoice::None, _) => {
            tools.clear();
            None
        }
    };
    let (max_tokens, max_completion_tokens) = summary_config.map_or_else(
        || output_token_cap_fields(provider),
        |config| {
            if provider.compat.max_completion_tokens {
                (None, Some(config.max_output_tokens()))
            } else {
                (Some(config.max_output_tokens()), None)
            }
        },
    );
    let request = ChatRequest {
        model: model.id.as_str().to_owned(),
        messages,
        stream: true,
        stream_options: provider.compat.stream_options.then_some(StreamOptions {
            include_usage: true,
        }),
        parallel_tool_calls: (provider.compat.parallel_tool_calls && !tools.is_empty())
            .then_some(true),
        prompt_cache_key: provider
            .compat
            .prompt_cache
            .is_some()
            .then(|| format!("tau:{}", prompt.agent_id)),
        prompt_cache_retention: match provider.compat.prompt_cache {
            Some(PromptCache::Legacy { retention }) => Some(retention.wire()),
            Some(PromptCache::ExplicitSystemPrompt) | None => None,
        },
        prompt_cache_options: explicit_system_prompt.then_some(PromptCacheOptions {
            mode: "explicit",
            ttl: "30m",
        }),
        reasoning_effort: provider
            .compat
            .reasoning_effort
            .map(|wire| effort_wire(prompt.model_params.effort, wire)),
        max_tokens,
        max_completion_tokens,
        extra_body: provider.extra_body.clone(),
        tools,
        tool_choice,
    };
    Ok(request)
}

fn with_admitted_historical_prefix<T>(
    summary_config: Option<LocalSummaryCompactionConfig>,
    context: &tau_proto::PromptContext,
    fits_budget: impl FnOnce(&tau_proto::PromptContext, tau_proto::ByteCount) -> Option<bool>,
    lower: impl FnOnce() -> Result<T, LlmError>,
) -> Result<T, LlmError> {
    if let Some(config) = summary_config {
        tau_provider::local_summary_compaction::validate_trailing_trigger(context)
            .map_err(|error| LlmError::InvalidCompaction(error.to_owned()))?;
        if let Some(budget) = config.max_input_bytes()
            && fits_budget(context, budget) != Some(true)
        {
            return Err(LlmError::InvalidCompaction(
                "summary compaction prefix exceeds the published safe budget".to_owned(),
            ));
        }
    }
    lower()
}

fn context_block_has_system_authority(block: &tau_proto::ContextBlock) -> bool {
    let tau_proto::ContextBlock::UserInput(block) = block else {
        return false;
    };
    block.items.iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(message)
                if matches!(message.role, ContextRole::System | ContextRole::Developer)
        )
    })
}

fn validate_extra_body(extra_body: &BTreeMap<String, serde_json::Value>) -> Result<(), LlmError> {
    const RESERVED: &[&str] = &[
        "model",
        "messages",
        "stream",
        "stream_options",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "prompt_cache_key",
        "prompt_cache_retention",
        "prompt_cache_options",
        "reasoning_effort",
        "max_tokens",
        "max_completion_tokens",
    ];
    if let Some(field) = RESERVED
        .iter()
        .find(|field| extra_body.contains_key(**field))
    {
        return Err(LlmError::ExtraBodyCollision((*field).to_owned()));
    }
    Ok(())
}

#[cfg(test)]
fn build_request(
    provider: &AttemptConfig,
    model: &AttemptModel,
    prompt: &tau_proto::AgentPromptCreated,
) -> ChatRequest {
    try_build_request(provider, model, prompt).expect("test request tools must be supported")
}

fn debug_file_prefix(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    provider_attempt: tau_proto::ProviderAttempt,
    wire_dispatch_index: u64,
) -> serde_json::Value {
    serde_json::json!({
        "session_id": prompt.session_id,
        "agent_prompt_id": prompt.agent_prompt_id,
        "transport": "http-sse",
        "backend": "chat_completions",
        "model": model.id,
        "operation": match prompt.operation {
            tau_proto::PromptOperation::Inference => "inference",
            tau_proto::PromptOperation::StandaloneCompaction => "compact",
        },
        "logical_attempt": provider_attempt.get(),
        "wire_dispatch_index": wire_dispatch_index,
    })
}

fn submit_debug_json_with(
    prompt: &tau_proto::AgentPromptCreated,
    class: tau_provider::debug_capture_writer::ProviderDebugCaptureClass,
    debug_provider_requests: bool,
    metadata: impl FnOnce() -> serde_json::Value,
    submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
) -> serde_json::Result<()> {
    if !debug_provider_requests {
        return Ok(());
    }
    let metadata = metadata();
    submit(
        path_tau_provider_debug_capture_writer::ProviderDebugCapture::new(
            prompt.session_id.clone(),
            prompt.agent_prompt_id.clone(),
            class,
            serde_json::to_vec_pretty(&metadata)?,
        ),
    );
    Ok(())
}

fn provider_request_debug_metadata(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    body: &ChatRequest,
    provider_attempt: tau_proto::ProviderAttempt,
    wire_dispatch_index: u64,
) -> serde_json::Value {
    let mut body = serde_json::to_value(body).expect("Chat request serializes");
    redact_image_data_urls(&mut body);
    serde_json::json!({
        "session_id": prompt.session_id,
        "agent_prompt_id": prompt.agent_prompt_id,
        "transport": "http-sse",
        "backend": "chat_completions",
        "model": model.id,
        "context_item_count": prompt.context.flatten_iter().count(),
        "tool_count": prompt.tools.len(),
        "tool_choice": prompt.tool_choice,
        "body": body,
        "operation": match prompt.operation {
            tau_proto::PromptOperation::Inference => "inference",
            tau_proto::PromptOperation::StandaloneCompaction => "compact",
        },
        "logical_attempt": provider_attempt.get(),
        "wire_dispatch_index": wire_dispatch_index,
    })
}

fn redact_image_data_urls(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) if text.starts_with("data:image/") => {
            *text = "[image data omitted]".to_owned();
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_image_data_urls(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                redact_image_data_urls(value);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

fn maybe_debug_submit_provider_request(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    debug_provider_requests: bool,
    body: &ChatRequest,
    provider_attempt: tau_proto::ProviderAttempt,
    wire_dispatch_index: u64,
) {
    maybe_debug_submit_provider_request_with(
        prompt,
        model,
        debug_provider_requests,
        body,
        provider_attempt,
        wire_dispatch_index,
        submit_provider_capture,
    );
}

fn maybe_debug_submit_provider_request_with(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    debug_provider_requests: bool,
    body: &ChatRequest,
    provider_attempt: tau_proto::ProviderAttempt,
    wire_dispatch_index: u64,
    submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
) {
    if let Err(error) = submit_debug_json_with(
        prompt,
        path_tau_provider_debug_capture_writer::ProviderDebugCaptureClass::HttpSseRequest,
        debug_provider_requests,
        || {
            provider_request_debug_metadata(
                prompt,
                model,
                body,
                provider_attempt,
                wire_dispatch_index,
            )
        },
        submit,
    ) {
        tracing::warn!(
            target: LOG_TARGET,
            session_id = %prompt.session_id,
            agent_prompt_id = %prompt.agent_prompt_id,
            "failed to serialize chat completions provider request debug capture: {error}",
        );
    }
}

fn maybe_debug_submit_provider_response(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    debug_provider_requests: bool,
    state: &StreamState,
    raw_events: &[serde_json::Value],
    provider_attempt: tau_proto::ProviderAttempt,
    wire_dispatch_index: u64,
) {
    maybe_debug_submit_provider_response_with(
        prompt,
        model,
        debug_provider_requests,
        state,
        raw_events,
        provider_attempt,
        wire_dispatch_index,
        submit_provider_capture,
    );
}

#[allow(clippy::too_many_arguments)] // Capture payload and injectable sink have distinct test ownership.
fn maybe_debug_submit_provider_response_with(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    debug_provider_requests: bool,
    state: &StreamState,
    raw_events: &[serde_json::Value],
    provider_attempt: tau_proto::ProviderAttempt,
    wire_dispatch_index: u64,
    submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
) {
    if let Err(error) = submit_debug_json_with(
        prompt,
        path_tau_provider_debug_capture_writer::ProviderDebugCaptureClass::HttpSseResponse,
        debug_provider_requests,
        || {
            provider_response_debug_metadata(
                prompt,
                model,
                state,
                raw_events,
                provider_attempt,
                wire_dispatch_index,
            )
        },
        submit,
    ) {
        tracing::warn!(
            target: LOG_TARGET,
            session_id = %prompt.session_id,
            agent_prompt_id = %prompt.agent_prompt_id,
            "failed to serialize chat completions provider response debug capture: {error}",
        );
    }
}

fn provider_response_debug_metadata(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    state: &StreamState,
    raw_events: &[serde_json::Value],
    provider_attempt: tau_proto::ProviderAttempt,
    wire_dispatch_index: u64,
) -> serde_json::Value {
    let mut metadata = debug_file_prefix(prompt, model, provider_attempt, wire_dispatch_index);
    if let serde_json::Value::Object(map) = &mut metadata {
        map.insert(
            "usage".to_owned(),
            serde_json::to_value(state.usage()).unwrap_or_default(),
        );
        map.insert(
            "stop_reason".to_owned(),
            serde_json::to_value(state.stop_reason).unwrap_or_default(),
        );
        map.insert(
            "output_items".to_owned(),
            serde_json::to_value(state.output_items()).unwrap_or_default(),
        );
        map.insert(
            "raw_events".to_owned(),
            serde_json::Value::Array(raw_events.to_vec()),
        );
    }
    metadata
}

fn maybe_debug_submit_provider_http_error(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    debug_provider_requests: bool,
    status: u16,
    body: &str,
    provider_attempt: tau_proto::ProviderAttempt,
    wire_dispatch_index: u64,
) {
    maybe_debug_submit_provider_http_error_with(
        prompt,
        model,
        debug_provider_requests,
        status,
        body,
        provider_attempt,
        wire_dispatch_index,
        submit_provider_capture,
    );
}

/// Finalize the one failure-side provider capture for an attempt.
fn finalize_provider_capture(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    debug_provider_requests: bool,
    provider_attempt: tau_proto::ProviderAttempt,
    wire_dispatch_index: Option<u64>,
    error: &LlmError,
) {
    let Some(wire_dispatch_index) = wire_dispatch_index.or_else(|| {
        matches!(
            error,
            LlmError::HttpStatus(..) | LlmError::HttpStatusHinted(..)
        )
        .then_some(1)
    }) else {
        return;
    };
    match error {
        LlmError::HttpStatus(status, body) | LlmError::HttpStatusHinted(status, body, _) => {
            maybe_debug_submit_provider_http_error(
                prompt,
                model,
                debug_provider_requests,
                *status,
                body,
                provider_attempt,
                wire_dispatch_index,
            );
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)] // Capture payload and injectable sink have distinct test ownership.
fn maybe_debug_submit_provider_http_error_with(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    debug_provider_requests: bool,
    status: u16,
    body: &str,
    provider_attempt: tau_proto::ProviderAttempt,
    wire_dispatch_index: u64,
    submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
) {
    if let Err(error) = submit_debug_json_with(
        prompt,
        path_tau_provider_debug_capture_writer::ProviderDebugCaptureClass::HttpSseResponse,
        debug_provider_requests,
        || {
            provider_http_error_debug_metadata(
                prompt,
                model,
                status,
                body,
                provider_attempt,
                wire_dispatch_index,
            )
        },
        submit,
    ) {
        tracing::warn!(
            target: LOG_TARGET,
            session_id = %prompt.session_id,
            agent_prompt_id = %prompt.agent_prompt_id,
            "failed to serialize chat completions provider HTTP-error debug capture: {error}",
        );
    }
}

fn provider_http_error_debug_metadata(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    status: u16,
    body: &str,
    provider_attempt: tau_proto::ProviderAttempt,
    wire_dispatch_index: u64,
) -> serde_json::Value {
    let mut metadata = debug_file_prefix(prompt, model, provider_attempt, wire_dispatch_index);
    if let serde_json::Value::Object(map) = &mut metadata {
        map.insert("http_status".to_owned(), serde_json::json!(status));
        map.insert("body".to_owned(), serde_json::json!(body));
    }
    metadata
}

fn reasoning_text_context_item(reasoning: &str) -> Option<ContextItem> {
    (!reasoning.is_empty()).then(|| {
        ContextItem::ReasoningText(ReasoningTextItem {
            kind: ReasoningTextKind::Full,
            text: reasoning.to_owned(),
        })
    })
}

fn output_token_cap_fields(provider: &AttemptConfig) -> (Option<u32>, Option<u32>) {
    if provider.max_output_tokens == 0 {
        return (None, None);
    }
    if provider.compat.max_completion_tokens {
        (None, Some(provider.max_output_tokens))
    } else {
        (Some(provider.max_output_tokens), None)
    }
}

fn append_context_block(
    block: &tau_proto::ContextBlock,
    reasoning_replay: ReasoningReplay,
    image_budget: &mut ImageRequestBudget,
    messages: &mut Vec<serde_json::Value>,
) {
    match block {
        tau_proto::ContextBlock::UserInput(block) => {
            for item in &block.items {
                let message = match item {
                    ContextItem::Message(message) => message.clone(),
                    _ => continue,
                };
                let text = message_text(&message);
                if text.is_empty() || message.role == ContextRole::User && text.trim().is_empty() {
                    continue;
                }
                messages.push(serde_json::json!({
                    "role": role_wire(&message.role),
                    "content": text,
                }));
            }
        }
        tau_proto::ContextBlock::AssistantResponse(block) => {
            let mut reasoning = String::new();
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            for item in &block.output_items {
                match item {
                    ContextItem::ReasoningText(item) if item.kind == ReasoningTextKind::Full => {
                        reasoning.push_str(&item.text);
                    }
                    ContextItem::ReasoningText(_) => {}
                    ContextItem::Reasoning(item) => {
                        if let Some(part) = chat_completions_reasoning_text(item) {
                            reasoning.push_str(&part);
                        }
                    }
                    ContextItem::Message(message) if message.role == ContextRole::Assistant => {
                        text.push_str(&message_text(message));
                    }
                    ContextItem::ToolCall(call) => {
                        tool_calls.push(serde_json::json!({
                            "id": call.call_id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": function_call_arguments_json(call),
                            }
                        }));
                    }
                    ContextItem::Message(_)
                    | ContextItem::ToolResult(_)
                    | ContextItem::LocalCompactionNarrative(_)
                    | ContextItem::CompactionTrigger
                    | ContextItem::Compaction(_)
                    | ContextItem::UnknownProviderItem(_) => {}
                }
            }
            if text.is_empty() && reasoning.is_empty() && tool_calls.is_empty() {
                return;
            }
            #[derive(Serialize)]
            struct AssistantReplayMessage {
                role: &'static str,
                content: Option<String>,
                #[serde(skip_serializing_if = "Option::is_none")]
                reasoning_content: Option<String>,
                #[serde(skip_serializing_if = "Option::is_none")]
                reasoning: Option<String>,
                #[serde(skip_serializing_if = "Vec::is_empty")]
                tool_calls: Vec<serde_json::Value>,
            }

            let reasoning = (!reasoning.is_empty()).then_some(reasoning);
            let reasoning_content = match reasoning_replay {
                ReasoningReplay::ReasoningContent | ReasoningReplay::Both => reasoning.clone(),
                ReasoningReplay::Reasoning => None,
            };
            let reasoning = match reasoning_replay {
                ReasoningReplay::Reasoning | ReasoningReplay::Both => reasoning,
                ReasoningReplay::ReasoningContent => None,
            };
            messages.push(
                serde_json::to_value(AssistantReplayMessage {
                    role: "assistant",
                    content: (!text.is_empty()).then_some(text),
                    reasoning_content,
                    reasoning,
                    tool_calls,
                })
                .expect("assistant replay message serializes"),
            );
        }
        tau_proto::ContextBlock::ToolResults(block) => {
            for result in &block.items {
                let content = chat_completions_tool_result_content(result, image_budget);
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": result.call_id,
                    "content": content,
                }));
            }
        }
    }
}

fn chat_completions_tool_result_content(
    result: &tau_proto::ToolResultItem,
    image_budget: &mut ImageRequestBudget,
) -> serde_json::Value {
    let text = result.render_provider_text();
    if result.provider_content.is_empty() {
        return serde_json::Value::String(text);
    }
    if !image_budget.supports_image_tool_results {
        return serde_json::Value::String(format!(
            "{text}\n[image omitted: this Chat Completions route does not support native image tool output]"
        ));
    }

    let mut content = vec![serde_json::json!({"type": "text", "text": text})];
    for part in &result.provider_content {
        let tau_proto::ToolResultContentPart::Image(image) = part;
        if !image_budget.reserve(image) {
            content.push(serde_json::json!({
                "type": "text",
                "text": "[image omitted: aggregate provider image request limit exceeded]",
            }));
            continue;
        }
        let encoded = BASE64_STANDARD.encode(&image.data);
        content.push(serde_json::json!({
            "type": "image_url",
            "image_url": {
                "url": format!(
                    "data:{};base64,{encoded}",
                    image.media_type.mime_type()
                ),
                "detail": "high",
            },
        }));
    }
    serde_json::Value::Array(content)
}

#[cfg(test)]
fn tool_result_text(status: ToolResultStatus, output: &tau_proto::ToolResponse) -> String {
    tau_proto::ToolResultItem {
        call_id: String::new().into(),
        tool_type: ToolType::Function,
        status,
        output: output.clone(),
        presentation: Default::default(),
        provider_content: Vec::new(),
    }
    .render_provider_text()
}

fn function_call_arguments_json(call: &ToolCallItem) -> String {
    call.raw_arguments_json.clone().unwrap_or_else(|| {
        serde_json::to_string(&cbor_to_json(&call.arguments)).unwrap_or_default()
    })
}

fn chat_completions_reasoning_text(item: &OpaqueProviderItem) -> Option<String> {
    let value = cbor_to_json(item.value());
    if value.get("type").and_then(|value| value.as_str()) != Some("chat_completions_reasoning") {
        return None;
    }
    value
        .get("reasoning_content")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn message_text(message: &tau_proto::MessageItem) -> String {
    let mut text = String::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text: part }
            | ContentPart::SyntheticCompactionSummary { text: part }
            | ContentPart::HarnessInternalText { text: part } => {
                text.push_str(part);
            }
        }
    }
    text
}

fn role_wire(role: &ContextRole) -> &'static str {
    match role {
        ContextRole::System => "system",
        ContextRole::Developer => "system",
        ContextRole::User => "user",
        ContextRole::Assistant => "assistant",
    }
}

fn convert_tool_definition(tool: &ToolDefinition) -> Result<serde_json::Value, LlmError> {
    if tool.tool_type != ToolType::Function {
        return Err(LlmError::UnsupportedToolType(tool.tool_type));
    }
    Ok(serde_json::json!({
        "type": "function",
        "function": {
            "name": tool.model_visible_name.as_ref().unwrap_or(&tool.name),
            "description": tool.description,
            "parameters": tool.parameters,
        }
    }))
}

fn apply_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(), LlmError> {
    // Observe compact shape separately, but still run the ordinary parser and
    // callbacks. The extension's transient sampling seam is intentionally
    // unchanged until its separately approved boundary change.
    let compact_validation = state
        .compact_validator
        .as_mut()
        .map_or(Ok(()), |validator| validator.observe(event));
    let parsed = (|| {
        if let Some(usage) = event.get("usage") {
            capture_usage(state, usage);
        }
        apply_stream_error(event)?;
        let Some(choice) = first_stream_choice(event) else {
            return Ok(());
        };
        let delta = &choice["delta"];
        if let Err(error) = apply_text_delta(state, delta) {
            if state.semantic_progress == SemanticProgress::Parsed {
                on_update(state);
            }
            return Err(error);
        }
        // Preserve the semantic fact before a later parser for the same event can
        // fail (for example, repetitive tool arguments after accepted content).
        if state.semantic_progress == SemanticProgress::Parsed {
            on_update(state);
        }
        if let Err(error) = apply_tool_call_deltas(state, delta) {
            if state.semantic_progress == SemanticProgress::Parsed {
                on_update(state);
            }
            return Err(error);
        }
        apply_finish_reason(state, choice)?;
        if state.semantic_progress == SemanticProgress::Parsed {
            on_update(state);
        }
        Ok(())
    })();
    compact_validation.and(parsed)
}

fn apply_stream_error(event: &serde_json::Value) -> Result<(), LlmError> {
    let Some(error) = event.get("error") else {
        return Ok(());
    };
    if error.is_null() {
        return Ok(());
    }
    let Some(error) = error.as_object() else {
        return Err(LlmError::StreamError(StreamFailure {
            retry: Some(RetryDecision::new(RetryClass::Unknown)),
            failure_kind: None,
        }));
    };
    Err(LlmError::StreamError(classify_stream_error(error)))
}

fn classify_stream_error(error: &serde_json::Map<String, serde_json::Value>) -> StreamFailure {
    let classification = CanonicalIdentifierFamily::from_stream_error(error)
        .classified()
        .and_then(classify_structured_identifier)
        .unwrap_or_else(|| {
            error
                .get("code")
                .and_then(serde_json::Value::as_u64)
                .and_then(|status| u16::try_from(status).ok())
                .map_or(
                    ErrorClassification::Retry(RetryClass::Unknown),
                    ErrorClassification::from_numeric_status,
                )
        });
    match classification {
        ErrorClassification::Terminal(failure_kind) => StreamFailure {
            retry: None,
            failure_kind: Some(failure_kind),
        },
        ErrorClassification::Retry(class) => stream_retry(class),
    }
}

fn stream_retry(class: RetryClass) -> StreamFailure {
    StreamFailure {
        retry: Some(RetryDecision::new(class)),
        failure_kind: None,
    }
}

fn first_stream_choice(event: &serde_json::Value) -> Option<&serde_json::Value> {
    event["choices"]
        .as_array()
        .and_then(|choices| choices.first())
}

fn apply_text_delta(state: &mut StreamState, delta: &serde_json::Value) -> Result<bool, LlmError> {
    let mut changed = false;
    for key in ["reasoning_content", "reasoning", "thinking"] {
        if let Some(reasoning) = non_empty_str(&delta[key]) {
            state.append_reasoning_delta(reasoning)?;
            changed = true;
        }
    }
    if let Some(content) = non_empty_str(&delta["content"]) {
        changed |= append_content_delta(state, content)?;
    }
    Ok(changed)
}

fn apply_tool_call_deltas(
    state: &mut StreamState,
    delta: &serde_json::Value,
) -> Result<bool, LlmError> {
    let Some(tool_calls) = delta["tool_calls"].as_array() else {
        return Ok(false);
    };
    let mut changed = false;
    for tool_call in tool_calls {
        changed |= apply_tool_call_delta(state, tool_call)?;
    }
    Ok(changed)
}

fn apply_tool_call_delta(
    state: &mut StreamState,
    tool_call: &serde_json::Value,
) -> Result<bool, LlmError> {
    let index = tool_call["index"].as_u64().unwrap_or(0) as usize;
    let function = &tool_call["function"];
    let mut changed = update_tool_call_metadata(state, index, tool_call, function);
    if let Some(arguments) = function["arguments"].as_str() {
        state.append_tool_arguments_delta(index, arguments)?;
        changed = true;
    }
    Ok(changed)
}

fn update_tool_call_metadata(
    state: &mut StreamState,
    index: usize,
    tool_call: &serde_json::Value,
    function: &serde_json::Value,
) -> bool {
    let id = non_empty_str(&tool_call["id"]);
    let name = non_empty_str(&function["name"]);
    if id.is_some() || name.is_some() {
        state.semantic_progress = SemanticProgress::Parsed;
    }
    let (changed, accepted_new_name) = {
        let entry = state.tool_call_at_mut(index);
        let mut changed = false;
        if let Some(id) = id {
            entry.id = id.to_owned();
            changed = true;
        }
        let mut accepted_new_name = false;
        if let Some(name) = name {
            accepted_new_name = entry.name != name;
            entry.name = name.to_owned();
            changed = true;
        }
        (changed, accepted_new_name)
    };
    if accepted_new_name {
        state.record_deadline_semantic_progress();
    }
    changed
}

fn non_empty_str(value: &serde_json::Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn apply_finish_reason(
    state: &mut StreamState,
    choice: &serde_json::Value,
) -> Result<(), LlmError> {
    match choice.get("finish_reason") {
        None | Some(serde_json::Value::Null) => Ok(()),
        Some(serde_json::Value::String(reason)) => match reason.as_str() {
            "tool_calls" => {
                state.stop_reason = ProviderStopReason::ToolCalls;
                Ok(())
            }
            "stop" => {
                state.stop_reason = ProviderStopReason::EndTurn;
                Ok(())
            }
            "length" => {
                state.stop_reason = ProviderStopReason::Length;
                Ok(())
            }
            _ => Err(LlmError::UnknownFinishReason),
        },
        Some(_) => Err(LlmError::UnknownFinishReason),
    }
}

fn append_content_delta(state: &mut StreamState, content: &str) -> Result<bool, LlmError> {
    if !content.is_empty() {
        state.semantic_progress = SemanticProgress::Parsed;
    }
    state.pending_content.push_str(content);
    let mut changed = false;
    loop {
        if state.pending_content.is_empty() {
            return Ok(changed);
        }
        if state.in_think_tag {
            if let Some(index) = state.pending_content.find("</think>") {
                let reasoning = state.pending_content[..index].to_owned();
                state.append_reasoning_delta(&reasoning)?;
                state.pending_content.drain(..index + "</think>".len());
                state.in_think_tag = false;
                changed = true;
                continue;
            }
            let keep = partial_tag_suffix_len(&state.pending_content, "</think>");
            let emit_len = state.pending_content.len() - keep;
            if emit_len == 0 {
                return Ok(changed);
            }
            let reasoning = state.pending_content[..emit_len].to_owned();
            state.append_reasoning_delta(&reasoning)?;
            state.pending_content.drain(..emit_len);
            return Ok(true);
        }

        if let Some(index) = state.pending_content.find("<think>") {
            let text = state.pending_content[..index].to_owned();
            state.append_assistant_text_delta(&text)?;
            state.pending_content.drain(..index + "<think>".len());
            state.in_think_tag = true;
            changed = true;
            continue;
        }
        let keep = partial_tag_suffix_len(&state.pending_content, "<think>");
        let emit_len = state.pending_content.len() - keep;
        if emit_len == 0 {
            return Ok(changed);
        }
        let text = state.pending_content[..emit_len].to_owned();
        state.append_assistant_text_delta(&text)?;
        state.pending_content.drain(..emit_len);
        return Ok(true);
    }
}

fn partial_tag_suffix_len(text: &str, tag: &str) -> usize {
    let mut keep = 0;
    for len in 1..tag.len() {
        if text.ends_with(&tag[..len]) {
            keep = len;
        }
    }
    keep
}

fn flush_pending_content(
    state: &mut StreamState,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(), LlmError> {
    if state.pending_content.is_empty() {
        return Ok(());
    }
    if state.in_think_tag {
        let reasoning = state.pending_content.clone();
        state.append_reasoning_delta(&reasoning)?;
    } else {
        let text = state.pending_content.clone();
        state.append_assistant_text_delta(&text)?;
    }
    state.pending_content.clear();
    on_update(state);
    Ok(())
}

fn capture_usage(state: &mut StreamState, usage: &serde_json::Value) {
    state.input_tokens = usage["prompt_tokens"].as_u64();
    state.output_tokens = usage["completion_tokens"].as_u64();
    match state.cache_usage {
        CacheUsageCompat::None => {}
        CacheUsageCompat::OpenAi => {
            state.cached_tokens = usage["prompt_tokens_details"]["cached_tokens"].as_u64();
            state.cache_write_tokens = usage["prompt_tokens_details"]["cache_write_tokens"]
                .as_u64()
                .or_else(|| usage["cache_write_tokens"].as_u64());
        }
        CacheUsageCompat::DeepSeek => {
            state.cached_tokens = usage["prompt_cache_hit_tokens"].as_u64();
            state.cache_miss_tokens = usage["prompt_cache_miss_tokens"].as_u64();
        }
    }
}

/// Normalize potentially contradictory provider cache counters against total
/// input.
fn normalize_cache_usage(
    capability: CacheUsageCompat,
    input: u64,
    reads: Option<u64>,
    writes: Option<u64>,
    misses: Option<u64>,
) -> Option<Box<tau_proto::ProviderCacheUsage>> {
    if capability == CacheUsageCompat::None
        || (reads.is_none() && writes.is_none() && misses.is_none())
    {
        return None;
    }
    Some(Box::new(
        tau_proto::ProviderCacheUsage {
            read_tokens: reads,
            write_tokens: writes,
            miss_tokens: misses,
            cacheable_prefix_tokens: None,
            refresh_reason: Some(tau_proto::ProviderCacheRefreshReason::OrdinaryRequest),
            expiry_confidence: Some(match capability {
                CacheUsageCompat::DeepSeek => {
                    tau_proto::ProviderCacheExpiryConfidence::Probabilistic
                }
                CacheUsageCompat::OpenAi | CacheUsageCompat::None => {
                    tau_proto::ProviderCacheExpiryConfidence::Unknown
                }
            }),
            avoided_prefill_tokens: reads,
            storage_token_micros: None,
        }
        .normalized(input),
    ))
}

fn bounded_provider_error(text: &str) -> String {
    const MAX_CHARS: usize = 512;
    let mut out = text.chars().take(MAX_CHARS).collect::<String>();
    if text.chars().nth(MAX_CHARS).is_some() {
        out.push('…');
    }
    out
}

fn assistant_text_item(text: impl Into<String>) -> ContextItem {
    ContextItem::Message(tau_proto::MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
        responses_raw_json: None,
    })
}

fn effort_wire(effort: tau_proto::Effort, wire: ReasoningEffortWire) -> &'static str {
    match (effort, wire) {
        (tau_proto::Effort::XHigh, ReasoningEffortWire::Literal) => "xhigh",
        (tau_proto::Effort::Max, ReasoningEffortWire::Literal) => "max",
        (effort, _) => match effort {
            tau_proto::Effort::Off => "none",
            tau_proto::Effort::Minimal => "minimal",
            tau_proto::Effort::Low => "low",
            tau_proto::Effort::Medium => "medium",
            tau_proto::Effort::High => "high",
            tau_proto::Effort::XHigh | tau_proto::Effort::Max => "high",
        },
    }
}

fn cbor_to_json(value: &tau_proto::CborValue) -> serde_json::Value {
    match value {
        tau_proto::CborValue::Null => serde_json::Value::Null,
        tau_proto::CborValue::Bool(v) => serde_json::Value::Bool(*v),
        tau_proto::CborValue::Integer(v) => {
            let n: i128 = (*v).into();
            serde_json::json!(n)
        }
        tau_proto::CborValue::Float(v) => serde_json::Number::from_f64(*v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        tau_proto::CborValue::Text(v) => serde_json::Value::String(v.clone()),
        tau_proto::CborValue::Bytes(bytes) => serde_json::Value::Array(
            bytes
                .iter()
                .map(|byte| serde_json::Value::Number((*byte).into()))
                .collect(),
        ),
        tau_proto::CborValue::Array(items) => {
            serde_json::Value::Array(items.iter().map(cbor_to_json).collect())
        }
        tau_proto::CborValue::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (key, value) in entries {
                let key = match key {
                    tau_proto::CborValue::Text(text) => text.clone(),
                    other => serde_json::to_string(&cbor_to_json(other)).unwrap_or_default(),
                };
                map.insert(key, cbor_to_json(value));
            }
            serde_json::Value::Object(map)
        }
        tau_proto::CborValue::Tag(_, inner) => cbor_to_json(inner),
        _ => serde_json::Value::Null,
    }
}

fn json_to_cbor(value: &serde_json::Value) -> tau_proto::CborValue {
    match value {
        serde_json::Value::Null => tau_proto::CborValue::Null,
        serde_json::Value::Bool(v) => tau_proto::CborValue::Bool(*v),
        serde_json::Value::Number(v) => {
            if let Some(n) = v.as_i64() {
                tau_proto::CborValue::Integer(n.into())
            } else if let Some(n) = v.as_u64() {
                tau_proto::CborValue::Integer(n.into())
            } else if let Some(n) = v.as_f64() {
                tau_proto::CborValue::Float(n)
            } else {
                tau_proto::CborValue::Null
            }
        }
        serde_json::Value::String(v) => tau_proto::CborValue::Text(v.clone()),
        serde_json::Value::Array(items) => {
            tau_proto::CborValue::Array(items.iter().map(json_to_cbor).collect())
        }
        serde_json::Value::Object(map) => tau_proto::CborValue::Map(
            map.iter()
                .map(|(key, value)| (tau_proto::CborValue::Text(key.clone()), json_to_cbor(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests;
