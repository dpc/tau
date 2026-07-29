//! OpenAI-compatible Chat Completions backend helpers.
//!
//! Component responsibilities and provider/replay trust boundaries are
//! documented in `ARCH-tau-provider-chat-completions`.

use std::collections::{BTreeMap, HashMap};
#[cfg(test)]
use std::io::Read;
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use tau_proto::{
    ContentPart, ContextItem, ContextRole, ModelName, OpaqueProviderItem, ProviderStopReason,
    ProviderTokenUsage, ReasoningTextItem, ReasoningTextKind, ToolCallItem, ToolChoice,
    ToolDefinition, ToolResponseHeader, ToolResultStatus, ToolType,
};
use tau_provider::retry_policy::{
    RetryClass, RetryDecision, classify_error_code, parse_json_error_code, parse_json_reset_hint,
    parse_retry_after,
};
use tau_provider::{StreamRepetitionGuard, StreamRepetitionKey};

const LOG_TARGET: &str = "provider-chat-completions";
/// Default Chat Completions output-token cap Tau sends when no
/// provider-specific override is set.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;
const STREAM_READ_POLL_TIMEOUT: Duration = Duration::from_secs(1);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const ATTEMPT_PHASE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
const MAX_DEBUG_EVENTS: usize = 4096;
const MAX_HTTP_ERROR_BODY_BYTES: u64 = 64 * 1024;
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
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
    /// Non-standard, non-conflicting request members.
    pub extra_body: BTreeMap<String, serde_json::Value>,
    /// Optional OpenAI-compatible request fields supported by this route.
    pub compat: AttemptCompat,
}

/// Wire capabilities selected by the extension for one attempt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AttemptCompat {
    /// Send `stream_options.include_usage`.
    pub stream_options: bool,
    /// Send `parallel_tool_calls` when tools are present.
    pub parallel_tool_calls: bool,
    /// Send an agent-derived prompt cache key.
    pub prompt_cache_key: bool,
    /// Send the selected reasoning effort.
    pub reasoning_effort: bool,
    /// Use `max_completion_tokens` rather than `max_tokens`.
    pub max_completion_tokens: bool,
}

/// Model wire identity for one attempt.
#[derive(Clone, Debug)]
pub struct AttemptModel {
    /// Upstream model id sent in the request.
    pub id: ModelName,
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
    ExtraBodyCollision(String),
    StreamError(StreamFailure),
}

#[derive(Debug)]
struct StreamFailure {
    retry: Option<RetryDecision>,
    failure_kind: Option<tau_proto::ProviderFailureKind>,
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
            Self::ExtraBodyCollision(_) => Some(tau_proto::ProviderFailureKind::RequestRejected),
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
/// Only canonical context rejection and deterministic status classes prove that
/// replaying unchanged input is futile; arbitrary codes and prose remain hints.
fn retry_decision_for_http_error(
    status: u16,
    body: &str,
    header_hint: Option<Duration>,
) -> Option<RetryDecision> {
    let provider_code = parse_json_error_code(body);
    if http_failure_kind(status, body).is_some() {
        return None;
    }
    let class = provider_code
        .as_deref()
        .map(classify_error_code)
        .filter(|class| *class != RetryClass::Unknown)
        .unwrap_or(match status {
            408 | 425 => RetryClass::Transport,
            429 => RetryClass::Throttle,
            500..=599 => RetryClass::Overload,
            401 | 403 => RetryClass::Auth,
            _ => RetryClass::Unknown,
        });
    let body_hint = parse_json_reset_hint(body, SystemTime::now());
    Some(RetryDecision::new(class).with_retry_after(header_hint.into_iter().chain(body_hint).max()))
}

fn http_failure_kind(status: u16, body: &str) -> Option<tau_proto::ProviderFailureKind> {
    let identifiers = canonical_error_identifiers(body);
    if identifiers
        .iter()
        .any(|code| code == "context_length_exceeded")
    {
        return Some(tau_proto::ProviderFailureKind::ContextWindowExceeded);
    }
    let known_transient = identifiers
        .iter()
        .any(|code| classify_error_code(code) != RetryClass::Unknown);
    if !known_transient && matches!(status, 400 | 404 | 409 | 413 | 422) {
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    } else {
        None
    }
}

fn canonical_error_identifiers(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    value
        .get("error")
        .into_iter()
        .flat_map(|error| {
            ["code", "type"]
                .into_iter()
                .filter_map(|field| error[field].as_str())
        })
        .map(ToOwned::to_owned)
        .collect()
}

/// Successful parsed result of one finite backend attempt.
#[derive(Debug)]
pub struct AttemptSuccess {
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
}

/// Typed terminal backend failure.
#[derive(Debug)]
pub struct AttemptFailure {
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
    },
    /// The harness canceled the active attempt.
    Canceled {
        /// Semantic progress parsed before cancellation.
        progress: SemanticProgress,
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
    let mut progress = SemanticProgress::None;
    let result = {
        let on_attempt_update = std::cell::RefCell::new(on_update);
        let mut on_state_update = |state: &StreamState| {
            let snapshot = AttemptProgress { state };
            progress = snapshot.semantic_progress();
            on_attempt_update.borrow_mut()(AttemptUpdate::Progress(snapshot));
        };
        let mut on_dispatched = |at| {
            on_attempt_update.borrow_mut()(AttemptUpdate::Dispatched(at));
        };
        chat_completions_stream(
            config,
            model,
            prompt,
            debug_provider_requests,
            &mut on_state_update,
            &mut on_dispatched,
            is_canceled,
            network,
        )
    };
    finish_attempt(result, progress)
}

fn finish_attempt(
    result: Result<StreamState, LlmError>,
    progress: SemanticProgress,
) -> AttemptOutcome {
    match result {
        Ok(state) => AttemptOutcome::Completed(AttemptSuccess {
            progress_items: state.indexed_output_items(),
            output_items: state.output_items(),
            stop_reason: state.stop_reason,
            usage: state.usage(),
            response_bytes_received: state.response_bytes_received(),
        }),
        Err(LlmError::Canceled) => AttemptOutcome::Canceled { progress },
        Err(error) => match error.retry_decision() {
            Some(decision) => AttemptOutcome::Retryable { decision, progress },
            None => AttemptOutcome::Terminal(AttemptFailure {
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
    output_tokens: Option<u64>,
    stop_reason: ProviderStopReason,
    repetition_guard: StreamRepetitionGuard,
    transport_response_bytes: u64,
    semantic_progress: SemanticProgress,
}

impl StreamState {
    fn new() -> Self {
        Self {
            text: String::new(),
            thinking: String::new(),
            output_items: Vec::new(),
            pending_content: String::new(),
            in_think_tag: false,
            tool_call_output_indices: HashMap::new(),
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            stop_reason: ProviderStopReason::EndTurn,
            repetition_guard: StreamRepetitionGuard::new(),
            transport_response_bytes: 0,
            semantic_progress: SemanticProgress::None,
        }
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
        Ok(())
    }

    fn append_reasoning_delta(&mut self, delta: &str) -> Result<(), LlmError> {
        if delta.is_empty() {
            return Ok(());
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
        Ok(())
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
        let cached = self.cached_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        Some(ProviderTokenUsage {
            model: None,
            prompt_sent_tokens: input,
            prompt_cached_tokens: cached,
            prompt_cache_read_ceiling_tokens: None,
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
    raw_events: &mut Vec<serde_json::Value>,
    on_update: &mut impl FnMut(&StreamState),
    is_canceled: &mut impl FnMut() -> bool,
) -> Result<(), LlmError> {
    let mut buffer = [0; 8192];
    let mut pending = Vec::new();
    let mut last_event_at = Instant::now();
    loop {
        if is_canceled() {
            return Err(LlmError::Canceled);
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                if !pending.is_empty() {
                    let line = decode_sse_line(std::mem::take(&mut pending));
                    let _done = apply_chat_stream_lines(vec![line], state, raw_events, on_update)?;
                }
                return Ok(());
            }
            Ok(bytes) => {
                let (done, progress) = process_stream_chunk(
                    &buffer[..bytes],
                    &mut pending,
                    state,
                    raw_events,
                    on_update,
                )?;
                if progress {
                    last_event_at = Instant::now();
                }
                if done {
                    return Ok(());
                }
                if last_event_at.elapsed() >= STREAM_IDLE_TIMEOUT {
                    return Err(LlmError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "provider stream idle timeout",
                    )));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                if last_event_at.elapsed() >= STREAM_IDLE_TIMEOUT {
                    return Err(LlmError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "provider stream idle timeout",
                    )));
                }
            }
            Err(error) => return Err(LlmError::Io(error)),
        }
    }
}

fn process_stream_chunk(
    bytes: &[u8],
    pending: &mut Vec<u8>,
    state: &mut StreamState,
    raw_events: &mut Vec<serde_json::Value>,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(bool, bool), LlmError> {
    state.record_transport_response_bytes(bytes.len());
    if state.transport_response_bytes > MAX_RESPONSE_BYTES {
        return Err(LlmError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "provider response exceeds byte limit",
        )));
    }
    pending.extend_from_slice(bytes);
    if pending.len() > MAX_SSE_LINE_BYTES {
        return Err(LlmError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "provider SSE line exceeds limit",
        )));
    }
    let lines = drain_complete_lines(pending);
    let progress = sse_lines_have_provider_event(&lines);
    let done = apply_chat_stream_lines(lines, state, raw_events, on_update)?;
    on_update(state);
    Ok((done, progress))
}

fn sse_lines_have_provider_event(lines: &[String]) -> bool {
    lines.iter().any(|line| line.starts_with("data: "))
}

fn drain_complete_lines(pending: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(newline_index) = pending.iter().position(|byte| *byte == b'\n') {
        let line_bytes = pending.drain(..=newline_index).collect();
        lines.push(decode_sse_line(line_bytes));
    }
    lines
}

fn decode_sse_line(mut line_bytes: Vec<u8>) -> String {
    if line_bytes.last() == Some(&b'\n') {
        line_bytes.pop();
    }
    if line_bytes.last() == Some(&b'\r') {
        line_bytes.pop();
    }
    String::from_utf8_lossy(&line_bytes).into_owned()
}

fn apply_chat_stream_lines(
    lines: Vec<String>,
    state: &mut StreamState,
    raw_events: &mut Vec<serde_json::Value>,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<bool, LlmError> {
    for line in lines {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            return Ok(true);
        }
        let event: serde_json::Value = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(_) => continue,
        };
        if raw_events.len() < MAX_DEBUG_EVENTS {
            raw_events.push(event.clone());
        }
        apply_event(state, &event, on_update)?;
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)] // Dispatch and state callbacks have distinct timing ownership.
fn chat_completions_stream(
    provider: &AttemptConfig,
    model: &AttemptModel,
    prompt: &tau_proto::AgentPromptCreated,
    debug_provider_requests: bool,
    on_update: &mut impl FnMut(&StreamState),
    on_dispatched: &mut impl FnMut(Instant),
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<StreamState, LlmError> {
    if is_canceled() {
        return Err(LlmError::Canceled);
    }
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let body = try_build_request(provider, model, prompt)?;
    let body_str = serde_json::to_string(&body).map_err(LlmError::Json)?;
    maybe_debug_submit_provider_request(prompt, model, debug_provider_requests, &body);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(LlmError::Io)?;
    let (mut state, raw_events) = runtime.block_on(chat_completions_stream_async(
        AsyncAttemptContext {
            url: &url,
            provider,
            body: &body_str,
            prompt,
            model,
            debug_provider_requests,
        },
        on_update,
        on_dispatched,
        is_canceled,
        network,
    ))?;
    flush_pending_content(&mut state, on_update)?;
    maybe_debug_submit_provider_response(
        prompt,
        model,
        debug_provider_requests,
        &state,
        &raw_events,
    );
    ensure_non_empty_end_turn(state)
}

struct AsyncAttemptContext<'a> {
    /// Fully resolved Chat Completions endpoint.
    url: &'a str,
    /// Mutable-profile values resolved for this attempt.
    provider: &'a AttemptConfig,
    /// Serialized request body owned by the synchronous caller.
    body: &'a str,
    /// Logical prompt used for diagnostics.
    prompt: &'a tau_proto::AgentPromptCreated,
    /// Selected model used for diagnostics.
    model: &'a AttemptModel,
    /// Whether durable-session private diagnostics are allowed.
    debug_provider_requests: bool,
}

async fn chat_completions_stream_async(
    context: AsyncAttemptContext<'_>,
    on_update: &mut impl FnMut(&StreamState),
    on_dispatched: &mut impl FnMut(Instant),
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<(StreamState, Vec<serde_json::Value>), LlmError> {
    let client = network
        .client_for(context.url)
        .map_err(LlmError::Outbound)?;
    let mut request = client
        .post(context.url)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(context.body.to_owned());
    if !context.provider.api_key.trim().is_empty() {
        request = request.bearer_auth(&context.provider.api_key);
    }
    let mut send = Box::pin(request.send());
    let mut header_started_at = None;
    let mut response = loop {
        if is_canceled() {
            return Err(LlmError::Canceled);
        }
        let header_started_at = *header_started_at.get_or_insert_with(|| {
            let at = Instant::now();
            on_dispatched(at);
            at
        });
        if let Ok(result) = tokio::time::timeout(STREAM_READ_POLL_TIMEOUT, &mut send).await {
            break result.map_err(|error| {
                LlmError::Outbound(network.reqwest_error(
                    context.url,
                    tau_provider::OutboundPhase::Request,
                    &error,
                ))
            })?;
        }
        if header_started_at.elapsed() >= ATTEMPT_PHASE_TIMEOUT {
            return Err(LlmError::Outbound(
                network.deadline_error(context.url, tau_provider::OutboundPhase::Request),
            ));
        }
    };
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
        let mut last_body_progress = Instant::now();
        while bytes.len() < MAX_HTTP_ERROR_BODY_BYTES as usize {
            if is_canceled() {
                return Err(LlmError::Canceled);
            }
            match tokio::time::timeout(STREAM_READ_POLL_TIMEOUT, response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    last_body_progress = Instant::now();
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
            if last_body_progress.elapsed() >= ATTEMPT_PHASE_TIMEOUT {
                return Err(LlmError::Outbound(
                    network.deadline_error(context.url, tau_provider::OutboundPhase::Body),
                ));
            }
        }
        let body = String::from_utf8_lossy(&bytes).into_owned();
        maybe_debug_submit_provider_http_error(
            context.prompt,
            context.model,
            context.debug_provider_requests,
            code,
            &body,
        );
        return Err(match retry_after {
            Some(delay) => LlmError::HttpStatusHinted(code, body, delay),
            None => LlmError::HttpStatus(code, body),
        });
    }
    let mut state = StreamState::new();
    let mut raw_events = Vec::new();
    let mut pending = Vec::new();
    let mut last_event_at = Instant::now();
    loop {
        if is_canceled() {
            return Err(LlmError::Canceled);
        }
        match tokio::time::timeout(STREAM_READ_POLL_TIMEOUT, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let (done, progress) = process_stream_chunk(
                    &chunk,
                    &mut pending,
                    &mut state,
                    &mut raw_events,
                    on_update,
                )?;
                if progress {
                    last_event_at = Instant::now();
                }
                if done {
                    return Ok((state, raw_events));
                }
            }
            Ok(Ok(None)) => return Ok((state, raw_events)),
            Ok(Err(error)) => {
                return Err(LlmError::Outbound(network.reqwest_error(
                    context.url,
                    tau_provider::OutboundPhase::Body,
                    &error,
                )));
            }
            Err(_) => {}
        }
        if last_event_at.elapsed() >= STREAM_IDLE_TIMEOUT {
            return Err(LlmError::Outbound(
                network.deadline_error(context.url, tau_provider::OutboundPhase::Body),
            ));
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

fn try_build_request(
    provider: &AttemptConfig,
    model: &AttemptModel,
    prompt: &tau_proto::AgentPromptCreated,
) -> Result<ChatRequest, LlmError> {
    validate_extra_body(&provider.extra_body)?;
    let mut messages = Vec::new();
    if !prompt.system_prompt.trim().is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": prompt.system_prompt,
        }));
    }
    for block in &prompt.context.blocks {
        append_context_block(block, &mut messages);
    }
    let tools = prompt
        .tools
        .iter()
        .map(convert_tool_definition)
        .collect::<Result<Vec<_>, _>>()?;
    let tool_choice = match (prompt.tool_choice, tools.is_empty()) {
        (ToolChoice::None, _) => Some("none"),
        (ToolChoice::Auto, false) => Some("auto"),
        (ToolChoice::Auto, true) => None,
    };
    let (max_tokens, max_completion_tokens) = output_token_cap_fields(provider);
    Ok(ChatRequest {
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
            .prompt_cache_key
            .then(|| format!("tau:{}", prompt.agent_id)),
        reasoning_effort: provider
            .compat
            .reasoning_effort
            .then(|| effort_wire(prompt.model_params.effort)),
        max_tokens,
        max_completion_tokens,
        extra_body: provider.extra_body.clone(),
        tools,
        tool_choice,
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
) -> serde_json::Value {
    serde_json::json!({
        "session_id": prompt.session_id,
        "agent_prompt_id": prompt.agent_prompt_id,
        "transport": "http-sse",
        "backend": "chat_completions",
        "model": model.id,
    })
}

fn submit_debug_json_with(
    prompt: &tau_proto::AgentPromptCreated,
    class: tau_provider::debug_capture_writer::ProviderDebugCaptureClass,
    debug_provider_requests: bool,
    metadata: &serde_json::Value,
    submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
) -> serde_json::Result<()> {
    if !debug_provider_requests {
        return Ok(());
    }
    submit(
        tau_provider::debug_capture_writer::ProviderDebugCapture::new(
            prompt.session_id.clone(),
            prompt.agent_prompt_id.clone(),
            class,
            serde_json::to_vec_pretty(metadata)?,
        ),
    );
    Ok(())
}

fn provider_request_debug_metadata(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    body: &ChatRequest,
) -> serde_json::Value {
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
    })
}

fn maybe_debug_submit_provider_request(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    debug_provider_requests: bool,
    body: &ChatRequest,
) {
    maybe_debug_submit_provider_request_with(
        prompt,
        model,
        debug_provider_requests,
        body,
        tau_provider::debug_capture_writer::submit_provider_debug_capture,
    );
}

fn maybe_debug_submit_provider_request_with(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    debug_provider_requests: bool,
    body: &ChatRequest,
    submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
) {
    let metadata = provider_request_debug_metadata(prompt, model, body);
    if let Err(error) = submit_debug_json_with(
        prompt,
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseRequest,
        debug_provider_requests,
        &metadata,
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
) {
    maybe_debug_submit_provider_response_with(
        prompt,
        model,
        debug_provider_requests,
        state,
        raw_events,
        tau_provider::debug_capture_writer::submit_provider_debug_capture,
    );
}

fn maybe_debug_submit_provider_response_with(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    debug_provider_requests: bool,
    state: &StreamState,
    raw_events: &[serde_json::Value],
    submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
) {
    let metadata = provider_response_debug_metadata(prompt, model, state, raw_events);
    if let Err(error) = submit_debug_json_with(
        prompt,
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseResponse,
        debug_provider_requests,
        &metadata,
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
) -> serde_json::Value {
    let mut metadata = debug_file_prefix(prompt, model);
    if let serde_json::Value::Object(map) = &mut metadata {
        map.insert(
            "usage".to_owned(),
            // Preserve behavior at this site.
            // ast-grep-ignore: unwrap-or-default
            serde_json::to_value(state.usage()).unwrap_or_default(),
        );
        map.insert(
            "stop_reason".to_owned(),
            // Preserve behavior at this site.
            // ast-grep-ignore: unwrap-or-default
            serde_json::to_value(state.stop_reason).unwrap_or_default(),
        );
        map.insert(
            "output_items".to_owned(),
            // Preserve behavior at this site.
            // ast-grep-ignore: unwrap-or-default
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
) {
    maybe_debug_submit_provider_http_error_with(
        prompt,
        model,
        debug_provider_requests,
        status,
        body,
        tau_provider::debug_capture_writer::submit_provider_debug_capture,
    );
}

fn maybe_debug_submit_provider_http_error_with(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
    debug_provider_requests: bool,
    status: u16,
    body: &str,
    submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
) {
    let metadata = provider_http_error_debug_metadata(prompt, model, status, body);
    if let Err(error) = submit_debug_json_with(
        prompt,
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseResponse,
        debug_provider_requests,
        &metadata,
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
) -> serde_json::Value {
    let mut metadata = debug_file_prefix(prompt, model);
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

fn append_context_block(block: &tau_proto::ContextBlock, messages: &mut Vec<serde_json::Value>) {
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
                #[serde(skip_serializing_if = "Vec::is_empty")]
                tool_calls: Vec<serde_json::Value>,
            }

            messages.push(
                serde_json::to_value(AssistantReplayMessage {
                    role: "assistant",
                    content: (!text.is_empty()).then_some(text),
                    reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                    tool_calls,
                })
                .expect("assistant replay message serializes"),
            );
        }
        tau_proto::ContextBlock::ToolResults(block) => {
            for result in &block.items {
                let mut content = tool_result_text(result.status.clone(), &result.output);
                if !result.provider_content.is_empty() {
                    content.push_str(
                        "\n[image omitted: Chat Completions does not support native image tool output]",
                    );
                }
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": result.call_id,
                    "content": content,
                }));
            }
        }
    }
}

fn function_call_arguments_json(call: &ToolCallItem) -> String {
    call.raw_arguments_json.clone().unwrap_or_else(|| {
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: unwrap-or-default
        serde_json::to_string(&cbor_to_json(&call.arguments)).unwrap_or_default()
    })
}

fn chat_completions_reasoning_text(item: &OpaqueProviderItem) -> Option<String> {
    let value = cbor_to_json(&item.value);
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
            ContentPart::Text { text: part } => text.push_str(part),
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

fn tool_result_text(status: ToolResultStatus, output: &tau_proto::ToolResponse) -> String {
    match status {
        ToolResultStatus::Success => output.render(),
        ToolResultStatus::Error { message } => {
            let mut response = output.clone();
            response.headers.insert(
                0,
                ToolResponseHeader {
                    key: "error".to_owned(),
                    value: message,
                },
            );
            response.render()
        }
        ToolResultStatus::Cancelled { reason } => tau_proto::ToolResponse {
            raw: tau_proto::CborValue::Null,
            headers: vec![ToolResponseHeader {
                key: "cancelled".to_owned(),
                value: reason,
            }],
            body: String::new(),
        }
        .render(),
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
    apply_finish_reason(state, choice);
    if state.semantic_progress == SemanticProgress::Parsed {
        on_update(state);
    }
    Ok(())
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
    let identifiers = [
        error.get("code").and_then(serde_json::Value::as_str),
        error.get("type").and_then(serde_json::Value::as_str),
        error
            .get("metadata")
            .and_then(serde_json::Value::as_object)
            .and_then(|metadata| metadata.get("error_type"))
            .and_then(serde_json::Value::as_str),
    ];
    if identifiers
        .into_iter()
        .flatten()
        .any(|identifier| identifier == "context_length_exceeded")
    {
        return StreamFailure {
            retry: None,
            failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
        };
    }
    if let Some(class) = identifiers
        .into_iter()
        .flatten()
        .map(classify_error_code)
        .find(|class| *class != RetryClass::Unknown)
    {
        return StreamFailure {
            retry: Some(RetryDecision::new(class)),
            failure_kind: None,
        };
    }
    match error.get("code").and_then(serde_json::Value::as_u64) {
        Some(400 | 404 | 409 | 413 | 422) => StreamFailure {
            retry: None,
            failure_kind: Some(tau_proto::ProviderFailureKind::RequestRejected),
        },
        Some(401 | 403) => stream_retry(RetryClass::Auth),
        Some(408 | 425) => stream_retry(RetryClass::Transport),
        Some(429) => stream_retry(RetryClass::Throttle),
        Some(500..=599) => stream_retry(RetryClass::Overload),
        _ => stream_retry(RetryClass::Unknown),
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
    let entry = state.tool_call_at_mut(index);
    let mut changed = false;
    if let Some(id) = id {
        entry.id = id.to_owned();
        changed = true;
    }
    if let Some(name) = name {
        entry.name = name.to_owned();
        changed = true;
    }
    changed
}

fn non_empty_str(value: &serde_json::Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn apply_finish_reason(state: &mut StreamState, choice: &serde_json::Value) {
    match choice["finish_reason"].as_str() {
        Some("tool_calls") => state.stop_reason = ProviderStopReason::ToolCalls,
        Some("stop") => state.stop_reason = ProviderStopReason::EndTurn,
        Some("length") => state.stop_reason = ProviderStopReason::Length,
        _ => {}
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
    state.cached_tokens = usage["prompt_tokens_details"]["cached_tokens"].as_u64();
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

fn effort_wire(effort: tau_proto::Effort) -> &'static str {
    match effort {
        tau_proto::Effort::Off => "none",
        tau_proto::Effort::Minimal => "minimal",
        tau_proto::Effort::Low => "low",
        tau_proto::Effort::Medium => "medium",
        tau_proto::Effort::High => "high",
        tau_proto::Effort::XHigh => "high",
        tau_proto::Effort::Max => "high",
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
                    // Preserve this behavior; the structural alternative is not semantics-neutral
                    // here. ast-grep-ignore: unwrap-or-default
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
