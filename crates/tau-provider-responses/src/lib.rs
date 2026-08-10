//! Generic public API-key Responses backend over HTTP/SSE or WebSocket.
//!
//! This intentionally does not share the private ChatGPT/Codex WebSocket
//! implementation. It sends a complete typed transcript on every turn.

mod deadlines;
mod websocket;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use tau_proto::{
    ContentPart, ContextItem, ContextRole, Effort, MessageItem, ModelName, ProviderStopReason,
    ProviderTokenUsage, ReasoningTextItem, ReasoningTextKind, ResponsesToolCallEnvelope,
    ToolCallItem, ToolChoice, ToolDefinition, ToolResponseHeader, ToolResultStatus, ToolType,
};
use tau_provider::retry_policy::{
    RetryClass, RetryDecision, classify_error_code, parse_json_error_code,
};
use tokio::runtime as path_tokio_runtime;

const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HTTP_ERROR_BODY_BYTES: u64 = 64 * 1024;
const MAX_EVENT_BYTES: usize = 1024 * 1024;
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Wire transport selected explicitly by a public Responses profile.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// HTTP request with a server-sent event response stream.
    #[default]
    Sse,
    /// One WebSocket connection and `response.create` exchange per finite
    /// attempt.
    Websocket,
}

/// Resolved endpoint configuration for one public Responses attempt.
#[derive(Clone)]
pub struct AttemptConfig {
    /// Base URL without the `/responses` suffix.
    pub base_url: String,
    /// Optional bearer credential.
    pub api_key: String,
    /// Requested output token cap, or zero to omit it.
    pub max_output_tokens: u32,
    /// Explicit wire transport; omitted profile values resolve to SSE.
    pub transport: Transport,
    /// Optional exact OpenAI prompt-cache policy selected for this route.
    pub prompt_cache: Option<PromptCachePolicy>,
}

/// Legacy OpenAI prompt-cache retention values supported by public Responses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptCacheRetention {
    /// Use the provider's ordinary in-memory retention behavior.
    InMemory,
    /// Request the provider's 24-hour retention behavior.
    Hours24,
}

/// One valid OpenAI prompt-cache policy for a public Responses route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptCachePolicy {
    /// Legacy automatic caching with a provider-selected retention policy.
    Legacy(PromptCacheRetention),
    /// Explicit caching at Tau's first typed input-text boundary.
    Explicit,
}

impl PromptCacheRetention {
    /// Return the exact legacy OpenAI wire spelling.
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::InMemory => "in_memory",
            Self::Hours24 => "24h",
        }
    }
}

/// Model wire identity for one Responses attempt.
#[derive(Clone, Debug)]
pub struct AttemptModel {
    /// Upstream model identifier.
    pub id: ModelName,
}

/// A stable output slot used by the extension sampler.
#[derive(Clone, Debug)]
pub struct AttemptOutputItem {
    /// Provider output index.
    pub output_index: u32,
    /// Typed semantic item.
    pub item: ContextItem,
}

/// Parsed successful Responses attempt.
#[derive(Debug)]
pub struct AttemptSuccess {
    /// Completed semantic output in provider order.
    pub output_items: Vec<ContextItem>,
    /// Tool-call or end-turn stop reason.
    pub stop_reason: ProviderStopReason,
    /// Reported token usage when present.
    pub usage: Option<ProviderTokenUsage>,
    /// Cumulative response body bytes.
    pub response_bytes_received: u64,
    /// Stable slots for progress rendering.
    pub progress_items: Vec<AttemptOutputItem>,
    /// Response identifier for diagnostics only, never chaining.
    pub provider_response_id: Option<String>,
}

/// Progress observed while parsing an attempt.
#[derive(Clone, Debug)]
pub struct AttemptProgress {
    /// Stable currently materialized items.
    pub output_items: Vec<AttemptOutputItem>,
    /// Cumulative transport bytes.
    pub response_bytes_received: u64,
    /// Whether this state qualifies as semantic output timing.
    pub has_timed_semantic_output: bool,
}

/// Result of one finite attempt.
#[derive(Debug)]
pub enum AttemptOutcome {
    /// The provider completed normally.
    Completed(AttemptSuccess),
    /// The extension may schedule a fresh full-replay attempt.
    Retryable {
        decision: RetryDecision,
        progress: AttemptProgress,
    },
    /// Cancellation won the attempt.
    Canceled { progress: AttemptProgress },
    /// A closed failure ended the prompt.
    Terminal(AttemptFailure),
}

/// Typed terminal failure from an attempt.
#[derive(Debug)]
pub struct AttemptFailure {
    /// Safe single-line diagnostic, with escaped bounded provider detail when
    /// available.
    pub message: String,
    /// Closed failure classification when known.
    pub failure_kind: Option<tau_proto::ProviderFailureKind>,
    /// Stop reason for a failed response.
    pub stop_reason: ProviderStopReason,
    /// Progress parsed before the terminal failure.
    pub progress: AttemptProgress,
}

#[derive(Debug)]
enum Error {
    EmptyResponse,
    Canceled,
    Outbound(tau_provider::OutboundError),
    Http(u16, String),
    Provider {
        /// Optional status carried by a post-upgrade provider event.
        status: Option<u16>,
        /// Bounded provider error code, type, or incomplete reason.
        code: Option<String>,
    },
    Json,
    InvalidRequest,
    UnsupportedTool,
    UnsupportedOutput,
    StreamFailure,
}

impl Error {
    fn retry(&self) -> Option<RetryDecision> {
        match self {
            Self::Canceled
            | Self::InvalidRequest
            | Self::UnsupportedTool
            | Self::UnsupportedOutput => None,
            Self::Http(status, body) => {
                if failure_kind(*status, body).is_some() {
                    return None;
                }
                let class = parse_json_error_code(body)
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
                Some(RetryDecision::new(class))
            }
            Self::Provider { status, code } => {
                if code
                    .as_deref()
                    .and_then(provider_code_failure_kind)
                    .is_some()
                {
                    return None;
                }
                let class = code
                    .as_deref()
                    .map(classify_error_code)
                    .filter(|class| *class != RetryClass::Unknown)
                    .unwrap_or(match status {
                        Some(408 | 425) => RetryClass::Transport,
                        Some(429) => RetryClass::Throttle,
                        Some(500..=599) => RetryClass::Overload,
                        Some(401 | 403) => RetryClass::Auth,
                        _ => RetryClass::Unknown,
                    });
                if matches!(class, RetryClass::Auth)
                    || status.is_some_and(
                        |status| matches!(status, 400..=407 | 409..=424 | 426..=428 | 430..=499),
                    )
                {
                    None
                } else {
                    Some(RetryDecision::new(class))
                }
            }
            Self::Outbound(error) => Some(RetryDecision::new(match error.kind() {
                tau_provider::OutboundErrorKind::InvalidConfiguration
                | tau_provider::OutboundErrorKind::ProxyAuthentication => RetryClass::Auth,
                tau_provider::OutboundErrorKind::Transport
                | tau_provider::OutboundErrorKind::Deadline
                | tau_provider::OutboundErrorKind::Protocol => RetryClass::Transport,
            })),
            Self::EmptyResponse | Self::Json | Self::StreamFailure => {
                Some(RetryDecision::new(RetryClass::Unknown))
            }
        }
    }

    fn failure_kind(&self) -> Option<tau_proto::ProviderFailureKind> {
        match self {
            Self::Http(status, body) => failure_kind(*status, body),
            Self::Provider { status, code } => {
                if let Some(kind) = code.as_deref().and_then(provider_code_failure_kind) {
                    Some(kind)
                } else if status.is_some_and(|status| matches!(status, 400..=499))
                    || code
                        .as_deref()
                        .is_some_and(|code| classify_error_code(code) == RetryClass::Auth)
                {
                    Some(tau_proto::ProviderFailureKind::RequestRejected)
                } else {
                    None
                }
            }
            Self::InvalidRequest | Self::UnsupportedTool | Self::UnsupportedOutput => {
                Some(tau_proto::ProviderFailureKind::RequestRejected)
            }
            _ => None,
        }
    }
}

fn provider_code_failure_kind(code: &str) -> Option<tau_proto::ProviderFailureKind> {
    match code {
        "context_length_exceeded" => Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
        "invalid_request_error" | "invalid_request" | "model_not_found" => {
            Some(tau_proto::ProviderFailureKind::RequestRejected)
        }
        _ => None,
    }
}

fn failure_kind(status: u16, body: &str) -> Option<tau_proto::ProviderFailureKind> {
    if parse_json_error_code(body).as_deref() == Some("context_length_exceeded") {
        return Some(tau_proto::ProviderFailureKind::ContextWindowExceeded);
    }
    matches!(status, 400 | 401 | 403 | 404 | 409 | 413 | 422)
        .then_some(tau_proto::ProviderFailureKind::RequestRejected)
}

/// Runs one cancellable full-transcript public Responses attempt.
#[allow(clippy::too_many_arguments)]
pub fn run_attempt(
    prompt: &tau_proto::AgentPromptCreated,
    config: &AttemptConfig,
    model: &AttemptModel,
    on_update: &mut impl FnMut(AttemptProgress),
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
) -> AttemptOutcome {
    let initial = AttemptProgress {
        output_items: Vec::new(),
        response_bytes_received: 0,
        has_timed_semantic_output: false,
    };
    if is_canceled() {
        return AttemptOutcome::Canceled { progress: initial };
    }
    let body = match build_request(prompt, config, model) {
        Ok(body) => body,
        Err(error) => return terminal(error, initial),
    };
    let runtime = match path_tokio_runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return terminal(Error::StreamFailure, initial),
    };
    let result = runtime.block_on(stream(
        prompt,
        config,
        &body,
        on_update,
        is_canceled,
        network,
    ));
    match result {
        Ok(state) if state.terminal => {
            let progress = state.progress();
            let output_items = state.output_items();
            if state.has_incomplete_reasoning() {
                terminal(Error::UnsupportedOutput, progress)
            } else if output_items.is_empty() {
                terminal(Error::EmptyResponse, progress)
            } else {
                let stop_reason = if output_items
                    .iter()
                    .any(|item| matches!(item, ContextItem::ToolCall(_)))
                {
                    ProviderStopReason::ToolCalls
                } else {
                    ProviderStopReason::EndTurn
                };
                AttemptOutcome::Completed(AttemptSuccess {
                    progress_items: progress.output_items,
                    output_items,
                    stop_reason,
                    usage: state.usage,
                    response_bytes_received: state.bytes,
                    provider_response_id: state.response_id,
                })
            }
        }
        Ok(state) => terminal(Error::EmptyResponse, state.progress()),
        Err((Error::Canceled, progress)) => AttemptOutcome::Canceled { progress },
        Err((error, progress)) => match error.retry() {
            Some(decision) => AttemptOutcome::Retryable { decision, progress },
            None => terminal(error, progress),
        },
    }
}

fn terminal(error: Error, progress: AttemptProgress) -> AttemptOutcome {
    AttemptOutcome::Terminal(AttemptFailure {
        message: match &error {
            Error::Canceled => "request canceled".to_owned(),
            Error::UnsupportedTool => "Responses supports Function tools only".to_owned(),
            Error::UnsupportedOutput => {
                "Responses supports text, plain reasoning, and Function output only".to_owned()
            }
            Error::InvalidRequest => "Responses request was invalid".to_owned(),
            Error::Http(status, _) => format!("provider returned HTTP {status}"),
            Error::Provider { status, code } => {
                let detail = code.as_deref().map(tau_proto::visible_escape_metadata);
                match (status, detail) {
                    (Some(status), Some(detail)) => {
                        format!("provider returned WebSocket error {status} ({detail})")
                    }
                    (Some(status), None) => format!("provider returned WebSocket error {status}"),
                    (None, Some(detail)) => {
                        format!("provider returned WebSocket error ({detail})")
                    }
                    (None, None) => "provider returned a WebSocket error".to_owned(),
                }
            }
            _ => "Responses request failed".to_owned(),
        },
        failure_kind: error.failure_kind(),
        stop_reason: ProviderStopReason::EndTurn,
        progress,
    })
}

#[derive(Debug)]
struct Slot {
    /// Provider output index shared by durable and display projections.
    index: u32,
    /// Durable semantic item, or a placeholder before item completion.
    item: ContextItem,
    /// Full display reasoning accumulated for this provider output item.
    reasoning_text: Option<ReasoningTextItem>,
    /// Full reasoning text accumulated independently for each content part.
    reasoning_parts: BTreeMap<ReasoningContentIndex, ReasoningPart>,
    /// Immutable provider item family and reasoning lifecycle phase.
    state: SlotState,
    /// Provider reasoning identity captured when the item was added.
    reasoning_item_id: Option<ReasoningItemId>,
}

/// Validated provider identity for one plain reasoning output item.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ReasoningItemId(
    /// Exact upstream item-id text.
    String,
);

/// Validated provider content index for one plain reasoning part.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReasoningContentIndex(
    /// Exact upstream nonnegative content index.
    u32,
);

/// Accumulated text and lifecycle phase for one reasoning content part.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ReasoningPart {
    /// Append-only full text observed for this part.
    text: String,
    /// Whether the provider has terminalized this part.
    phase: ReasoningPartPhase,
}

/// Streaming lifecycle for one reasoning content part.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReasoningPartPhase {
    /// More deltas may append while this remains the newest part.
    Streaming,
    /// No more deltas or done events are accepted.
    Done,
}

/// Validated plain reasoning content and its full display projection.
struct PlainReasoning {
    /// Content text keyed by provider content index.
    parts: BTreeMap<ReasoningContentIndex, String>,
    /// Concatenated full reasoning shown under the thinking policy.
    display: Option<ReasoningTextItem>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputItemPhase {
    /// The provider announced an item whose streaming content is incomplete.
    Added,
    /// The provider supplied the authoritative final item shape.
    Completed,
    /// A terminal response array supplied a complete item without streaming.
    TerminalFallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    /// No provider item family has claimed this slot.
    Empty,
    /// Assistant message content owns this slot.
    Message,
    /// Function-call content owns this slot.
    FunctionCall,
    /// Plain reasoning streams display text but lacks durable completion.
    ReasoningAdded,
    /// Plain reasoning has an immutable durable replay authority.
    ReasoningCompleted,
}

impl Slot {
    fn new(index: u32) -> Self {
        Self {
            index,
            item: ContextItem::UnknownProviderItem(tau_proto::OpaqueProviderItem {
                value: tau_proto::CborValue::Null,
                raw_json: None,
            }),
            reasoning_text: None,
            reasoning_parts: BTreeMap::new(),
            state: SlotState::Empty,
            reasoning_item_id: None,
        }
    }

    fn reasoning_event_id(&self, event: &Value) -> Result<ReasoningItemId, Error> {
        let event_id = event
            .get("item_id")
            .and_then(Value::as_str)
            .map(|id| ReasoningItemId(id.to_owned()))
            .ok_or(Error::UnsupportedOutput)?;
        if self.reasoning_item_id.is_some() && self.reasoning_item_id.as_ref() != Some(&event_id) {
            return Err(Error::UnsupportedOutput);
        }
        Ok(event_id)
    }

    fn append_reasoning_delta(
        &mut self,
        index: ReasoningContentIndex,
        delta: &str,
    ) -> Result<(), Error> {
        let newest = self
            .reasoning_parts
            .last_key_value()
            .map(|(index, _)| *index);
        if let Some(part) = self.reasoning_parts.get_mut(&index) {
            if newest != Some(index) || part.phase == ReasoningPartPhase::Done {
                return Err(Error::UnsupportedOutput);
            }
            part.text.push_str(delta);
        } else {
            if newest.is_some_and(|newest| !(newest < index)) {
                return Err(Error::UnsupportedOutput);
            }
            self.reasoning_parts.insert(
                index,
                ReasoningPart {
                    text: delta.to_owned(),
                    phase: ReasoningPartPhase::Streaming,
                },
            );
        }
        self.reasoning_text
            .get_or_insert_with(|| ReasoningTextItem {
                kind: ReasoningTextKind::Full,
                text: String::new(),
            })
            .text
            .push_str(delta);
        Ok(())
    }

    fn complete_reasoning_part(
        &mut self,
        index: ReasoningContentIndex,
        text: &str,
    ) -> Result<(), Error> {
        let newest = self
            .reasoning_parts
            .last_key_value()
            .map(|(index, _)| *index);
        if let Some(part) = self.reasoning_parts.get_mut(&index) {
            if part.phase == ReasoningPartPhase::Done || part.text != text {
                return Err(Error::UnsupportedOutput);
            }
            part.phase = ReasoningPartPhase::Done;
            return Ok(());
        }
        if newest.is_some_and(|newest| !(newest < index)) {
            return Err(Error::UnsupportedOutput);
        }
        self.reasoning_parts.insert(
            index,
            ReasoningPart {
                text: text.to_owned(),
                phase: ReasoningPartPhase::Done,
            },
        );
        if !text.is_empty() {
            self.reasoning_text
                .get_or_insert_with(|| ReasoningTextItem {
                    kind: ReasoningTextKind::Full,
                    text: String::new(),
                })
                .text
                .push_str(text);
        }
        Ok(())
    }

    fn apply_item(
        &mut self,
        item: &Value,
        phase: OutputItemPhase,
        raw_json: Option<&RawValue>,
    ) -> Result<bool, Error> {
        let qualifying_progress = match item["type"].as_str().unwrap_or("") {
            "message" if item["role"].as_str() == Some("assistant") => {
                if !matches!(self.state, SlotState::Empty | SlotState::Message) {
                    return Err(Error::UnsupportedOutput);
                }
                if !is_text_assistant_message(item) {
                    return Err(Error::UnsupportedOutput);
                }
                let mut message = MessageItem {
                    role: ContextRole::Assistant,
                    content: Vec::new(),
                    phase: None,
                    responses_raw_json: Some(
                        raw_json.map_or_else(|| item.to_string(), |raw| raw.get().to_owned()),
                    ),
                };
                if let Some(parts) = item["content"].as_array() {
                    for part in parts {
                        if matches!(part["type"].as_str(), Some("output_text") | Some("text"))
                            && let Some(text) = part["text"].as_str()
                        {
                            append_text(&mut message, text);
                        }
                    }
                }
                let changed = match &self.item {
                    ContextItem::Message(previous) => previous.content != message.content,
                    _ => true,
                };
                let qualifies = changed
                    && message
                        .content
                        .iter()
                        .any(|part| matches!(part, ContentPart::Text { text } if !text.is_empty()));
                self.item = ContextItem::Message(message);
                self.state = SlotState::Message;
                qualifies
            }
            "function_call" => {
                if !matches!(self.state, SlotState::Empty | SlotState::FunctionCall) {
                    return Err(Error::UnsupportedOutput);
                }
                let arguments = item["arguments"].as_str().unwrap_or("{}");
                let call_id = item["call_id"].as_str().ok_or(Error::InvalidRequest)?;
                let name = item["name"].as_str().ok_or(Error::InvalidRequest)?;
                let call = ToolCallItem {
                    call_id: tau_proto::ToolCallId::new(call_id),
                    name: tau_proto::ToolName::try_new(name.to_owned())
                        .ok_or(Error::InvalidRequest)?,
                    tool_type: ToolType::Function,
                    arguments: if arguments.is_empty() {
                        tau_proto::CborValue::Null
                    } else {
                        tau_proto::json_to_cbor(
                            &serde_json::from_str::<Value>(arguments).map_err(|_| Error::Json)?,
                        )
                    },
                    raw_arguments_json: Some(arguments.to_owned()),
                    responses_envelope: Some(ResponsesToolCallEnvelope {
                        item_id: item["id"].as_str().map(ToOwned::to_owned),
                        status: item["status"].as_str().map(ToOwned::to_owned),
                        extra_fields: tool_call_extra_fields(item),
                    }),
                };
                let changed = match &self.item {
                    ContextItem::ToolCall(previous) => {
                        previous.name != call.name
                            || previous.raw_arguments_json != call.raw_arguments_json
                    }
                    _ => true,
                };
                let qualifies = changed
                    && (!call.name.as_str().is_empty()
                        || call
                            .raw_arguments_json
                            .as_deref()
                            .is_some_and(|arguments| !arguments.is_empty()));
                self.item = ContextItem::ToolCall(call);
                self.state = SlotState::FunctionCall;
                qualifies
            }
            "reasoning" => {
                match phase {
                    OutputItemPhase::Added if self.state != SlotState::Empty => {
                        return Err(Error::UnsupportedOutput);
                    }
                    OutputItemPhase::Completed if self.state != SlotState::ReasoningAdded => {
                        return Err(Error::UnsupportedOutput);
                    }
                    OutputItemPhase::TerminalFallback if self.state != SlotState::Empty => {
                        return Err(Error::UnsupportedOutput);
                    }
                    OutputItemPhase::Added
                    | OutputItemPhase::Completed
                    | OutputItemPhase::TerminalFallback => {}
                }
                let item_id = reasoning_item_id(item)?;
                if self.reasoning_item_id.is_some() && self.reasoning_item_id != item_id {
                    return Err(Error::UnsupportedOutput);
                }
                let final_reasoning = plain_reasoning(item, phase)?;
                if phase == OutputItemPhase::Completed {
                    for (index, part) in &self.reasoning_parts {
                        if final_reasoning.parts.get(index) != Some(&part.text) {
                            return Err(Error::UnsupportedOutput);
                        }
                    }
                }
                self.reasoning_item_id = item_id;
                let part_phase = if phase == OutputItemPhase::Added {
                    ReasoningPartPhase::Streaming
                } else {
                    ReasoningPartPhase::Done
                };
                self.reasoning_parts = final_reasoning
                    .parts
                    .into_iter()
                    .map(|(index, text)| {
                        (
                            index,
                            ReasoningPart {
                                text,
                                phase: part_phase,
                            },
                        )
                    })
                    .collect();
                self.reasoning_text = final_reasoning.display;
                if matches!(
                    phase,
                    OutputItemPhase::Completed | OutputItemPhase::TerminalFallback
                ) {
                    self.item = ContextItem::Reasoning(tau_proto::OpaqueProviderItem {
                        value: tau_proto::json_to_cbor(item),
                        raw_json: Some(
                            raw_json.map_or_else(|| item.to_string(), |raw| raw.get().to_owned()),
                        ),
                    });
                    self.state = SlotState::ReasoningCompleted;
                } else {
                    self.state = SlotState::ReasoningAdded;
                }
                matches!(
                    phase,
                    OutputItemPhase::Completed | OutputItemPhase::TerminalFallback
                ) || self
                    .reasoning_text
                    .as_ref()
                    .is_some_and(|reasoning| !reasoning.text.is_empty())
            }
            _ => return Err(Error::UnsupportedOutput),
        };
        Ok(qualifying_progress)
    }
}

#[derive(Debug, Default)]
struct State {
    /// Provider-indexed slots in first-observed order.
    ///
    /// Plain reasoning may populate display text before an opaque durable item
    /// exists. Only a validated completed item turns that pending slot into the
    /// ordered full-display/durable pair returned by [`Self::output_items`].
    /// Terminalization rejects any remaining pending slot.
    items: Vec<Slot>,
    bytes: u64,
    terminal: bool,
    usage: Option<ProviderTokenUsage>,
    response_id: Option<String>,
    /// Monotonic revision advanced only by qualifying semantic observations.
    semantic_progress_revision: u64,
}

/// Raw JSON projections retained alongside semantically parsed SSE events.
#[derive(Deserialize)]
struct RawEvent<'a> {
    /// Exact output item carried by an added or done event.
    #[serde(default, borrow)]
    item: Option<&'a RawValue>,
    /// Exact terminal response envelope when nested under `response`.
    #[serde(default, borrow)]
    response: Option<RawResponse<'a>>,
    /// Exact terminal output when the event itself is the response envelope.
    #[serde(default, borrow)]
    output: Option<Vec<&'a RawValue>>,
}

/// Borrowed terminal response fields whose exact item syntax must survive.
#[derive(Deserialize)]
struct RawResponse<'a> {
    /// Exact ordered provider output items.
    #[serde(default, borrow)]
    output: Option<Vec<&'a RawValue>>,
}

impl State {
    fn output_items(&self) -> Vec<ContextItem> {
        self.items
            .iter()
            .flat_map(|slot| {
                let reasoning_text = matches!(slot.item, ContextItem::Reasoning(_))
                    .then(|| slot.reasoning_text.as_ref())
                    .flatten()
                    .filter(|item| !item.text.is_empty())
                    .cloned()
                    .map(ContextItem::ReasoningText);
                reasoning_text.into_iter().chain(
                    (!matches!(slot.item, ContextItem::UnknownProviderItem(_)))
                        .then(|| slot.item.clone()),
                )
            })
            .collect()
    }

    fn progress(&self) -> AttemptProgress {
        AttemptProgress {
            output_items: self
                .items
                .iter()
                .flat_map(|slot| {
                    let reasoning_text = slot
                        .reasoning_text
                        .as_ref()
                        .filter(|item| !item.text.is_empty())
                        .cloned()
                        .map(|item| AttemptOutputItem {
                            output_index: slot.index,
                            item: ContextItem::ReasoningText(item),
                        });
                    reasoning_text.into_iter().chain(
                        (!matches!(slot.item, ContextItem::UnknownProviderItem(_))).then(|| {
                            AttemptOutputItem {
                                output_index: slot.index,
                                item: slot.item.clone(),
                            }
                        }),
                    )
                })
                .collect(),
            response_bytes_received: self.bytes,
            has_timed_semantic_output: self.has_qualifying_stream_progress(),
        }
    }

    /// Returns whether this state contains material semantic stream output.
    ///
    /// This intentionally matches first-semantic-output timing: non-empty
    /// assistant or reasoning text, non-empty function name or arguments, and
    /// a completed opaque reasoning item qualify. Transport bytes, item
    /// allocation, empty deltas, status/control events, and unknown events do
    /// not qualify.
    fn has_qualifying_stream_progress(&self) -> bool {
        self.items.iter().any(|slot| {
            slot.reasoning_text
                .as_ref()
                .is_some_and(|item| !item.text.is_empty())
                || match &slot.item {
                    ContextItem::Message(message) => message
                        .content
                        .iter()
                        .any(|part| matches!(part, ContentPart::Text { text } if !text.is_empty())),
                    ContextItem::ToolCall(call) => {
                        !call.name.as_str().is_empty()
                            || call
                                .raw_arguments_json
                                .as_deref()
                                .is_some_and(|value| !value.is_empty())
                    }
                    ContextItem::Reasoning(_) => slot.state == SlotState::ReasoningCompleted,
                    _ => false,
                }
        })
    }

    fn has_incomplete_reasoning(&self) -> bool {
        self.items
            .iter()
            .any(|slot| slot.state == SlotState::ReasoningAdded)
    }

    fn slot_mut(&mut self, index: u32) -> &mut Slot {
        if let Some(position) = self.items.iter().position(|slot| slot.index == index) {
            return &mut self.items[position];
        }
        self.items.push(Slot::new(index));
        self.items.last_mut().expect("just appended slot")
    }

    fn apply_item_at(
        &mut self,
        index: u32,
        item: &Value,
        phase: OutputItemPhase,
        raw_json: Option<&RawValue>,
    ) -> Result<bool, Error> {
        if let Some(slot) = self.items.iter_mut().find(|slot| slot.index == index) {
            return slot.apply_item(item, phase, raw_json);
        }
        let mut slot = Slot::new(index);
        let qualifying_progress = slot.apply_item(item, phase, raw_json)?;
        self.items.push(slot);
        Ok(qualifying_progress)
    }

    fn existing_slot_mut(&mut self, index: u32) -> Result<&mut Slot, Error> {
        self.items
            .iter_mut()
            .find(|slot| slot.index == index)
            .ok_or(Error::UnsupportedOutput)
    }

    fn apply_event(&mut self, data: &str) -> Result<bool, Error> {
        let event: Value = serde_json::from_str(data).map_err(|_| Error::Json)?;
        let raw_event: RawEvent<'_> = serde_json::from_str(data).map_err(|_| Error::Json)?;
        let qualifying_progress = match event["type"].as_str().unwrap_or("") {
            "response.output_item.added" => {
                let index = output_index(&event)?;
                if let Some(item) = event.get("item") {
                    self.apply_item_at(index, item, OutputItemPhase::Added, raw_event.item)?
                } else {
                    false
                }
            }
            "response.output_item.done" => {
                let index = output_index(&event)?;
                if let Some(item) = event.get("item") {
                    self.apply_item_at(index, item, OutputItemPhase::Completed, raw_event.item)?
                } else {
                    false
                }
            }
            "response.output_text.delta" | "response.content_part.delta" => {
                let index = output_index(&event)?;
                let delta = event["delta"].as_str().unwrap_or("");
                if !delta.is_empty() {
                    let slot = self.slot_mut(index);
                    if !matches!(slot.state, SlotState::Empty | SlotState::Message) {
                        return Err(Error::UnsupportedOutput);
                    }
                    match &mut slot.item {
                        ContextItem::Message(message) => append_text(message, delta),
                        _ => {
                            slot.item = message_item(delta);
                            slot.state = SlotState::Message;
                        }
                    }
                }
                !delta.is_empty()
            }
            "response.function_call_arguments.delta" => {
                let index = output_index(&event)?;
                let delta = event["delta"].as_str().unwrap_or("");
                let slot = self.existing_slot_mut(index)?;
                if slot.state != SlotState::FunctionCall {
                    return Err(Error::UnsupportedOutput);
                }
                if let ContextItem::ToolCall(call) = &mut slot.item {
                    call.raw_arguments_json
                        .get_or_insert_with(String::new)
                        .push_str(delta);
                }
                !delta.is_empty()
            }
            "response.function_call_arguments.done" => {
                let index = output_index(&event)?;
                let arguments = event["arguments"]
                    .as_str()
                    .ok_or(Error::UnsupportedOutput)?;
                let parsed = tau_proto::json_to_cbor(
                    &serde_json::from_str::<Value>(arguments).map_err(|_| Error::Json)?,
                );
                let slot = self.existing_slot_mut(index)?;
                if slot.state != SlotState::FunctionCall {
                    return Err(Error::UnsupportedOutput);
                }
                if let ContextItem::ToolCall(call) = &mut slot.item {
                    let qualifying_progress = !arguments.is_empty()
                        && call.raw_arguments_json.as_deref() != Some(arguments);
                    call.raw_arguments_json = Some(arguments.to_owned());
                    call.arguments = parsed;
                    qualifying_progress
                } else {
                    false
                }
            }
            "response.reasoning_text.delta" => {
                let index = output_index(&event)?;
                let content_index = reasoning_content_index(&event)?;
                let delta = event["delta"].as_str().ok_or(Error::UnsupportedOutput)?;
                let slot = self.existing_slot_mut(index)?;
                if slot.state != SlotState::ReasoningAdded {
                    return Err(Error::UnsupportedOutput);
                }
                let item_id = slot.reasoning_event_id(&event)?;
                slot.append_reasoning_delta(content_index, delta)?;
                slot.reasoning_item_id = Some(item_id);
                !delta.is_empty()
            }
            "response.reasoning_text.done" => {
                let index = output_index(&event)?;
                let content_index = reasoning_content_index(&event)?;
                let text = event["text"].as_str().ok_or(Error::UnsupportedOutput)?;
                let slot = self.existing_slot_mut(index)?;
                if slot.state != SlotState::ReasoningAdded {
                    return Err(Error::UnsupportedOutput);
                }
                let item_id = slot.reasoning_event_id(&event)?;
                let has_text = slot
                    .reasoning_parts
                    .get(&content_index)
                    .is_some_and(|part| !part.text.is_empty());
                slot.complete_reasoning_part(content_index, text)?;
                slot.reasoning_item_id = Some(item_id);
                !has_text && !text.is_empty()
            }
            "response.completed" | "response.done" => {
                let response = event.get("response").unwrap_or(&event);
                let response_id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let usage = parse_usage(response.get("usage"));
                let mut replacement = None;
                if let Some(output) = response.get("output").and_then(Value::as_array) {
                    let raw_output = raw_event
                        .response
                        .and_then(|response| response.output)
                        .or(raw_event.output);
                    let mut terminal = State::default();
                    for (index, item) in output.iter().enumerate() {
                        terminal.apply_item_at(
                            index as u32,
                            item,
                            OutputItemPhase::TerminalFallback,
                            raw_output
                                .as_ref()
                                .and_then(|items| items.get(index))
                                .copied(),
                        )?;
                    }
                    let reasoning_disagrees = self.items.iter().any(|slot| {
                        matches!(
                            slot.state,
                            SlotState::ReasoningAdded | SlotState::ReasoningCompleted
                        ) && !terminal.items.iter().any(|terminal_slot| {
                            terminal_slot.index == slot.index
                                && reasoning_slots_agree(slot, terminal_slot)
                        })
                    });
                    if reasoning_disagrees {
                        return Err(Error::UnsupportedOutput);
                    }
                    replacement = Some(terminal.items);
                }
                if let Some(items) = replacement {
                    self.items = items;
                }
                self.response_id = response_id;
                self.usage = usage;
                self.terminal = true;
                false
            }
            "response.failed" | "response.incomplete" | "error" => {
                return Err(Error::StreamFailure);
            }
            _ => false,
        };
        let previous_revision = self.semantic_progress_revision;
        if qualifying_progress {
            self.semantic_progress_revision = self.semantic_progress_revision.saturating_add(1);
        }
        Ok(previous_revision < self.semantic_progress_revision)
    }
}

async fn stream(
    _prompt: &tau_proto::AgentPromptCreated,
    config: &AttemptConfig,
    body: &RequestBody,
    on_update: &mut impl FnMut(AttemptProgress),
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<State, (Error, AttemptProgress)> {
    match config.transport {
        Transport::Sse => stream_sse(config, body, on_update, is_canceled, network).await,
        Transport::Websocket => {
            websocket::stream(config, body, on_update, is_canceled, network).await
        }
    }
}

async fn stream_sse(
    config: &AttemptConfig,
    body: &RequestBody,
    on_update: &mut impl FnMut(AttemptProgress),
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<State, (Error, AttemptProgress)> {
    let url = format!("{}/responses", config.base_url.trim_end_matches('/'));
    let client = network
        .client_for(&url)
        .map_err(|error| (Error::Outbound(error), State::default().progress()))?;
    let serialized =
        serde_json::to_string(body).map_err(|_| (Error::Json, State::default().progress()))?;
    let mut request = client
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(serialized);
    if !config.api_key.trim().is_empty() {
        request = request.bearer_auth(&config.api_key);
    }
    let header_deadline = Instant::now() + deadlines::REQUEST_CONNECT_HEADER_TIMEOUT;
    let mut send = Box::pin(request.send());
    let response = loop {
        tokio::select! {
            response = &mut send => break response.map_err(|error| {
                (Error::Outbound(network.reqwest_error(
                    &url, tau_provider::OutboundPhase::Request, &error,
                )), State::default().progress())
            })?,
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(header_deadline)) => {
                return Err((Error::StreamFailure, State::default().progress()));
            }
            () = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {
                if is_canceled() {
                    return Err((Error::Canceled, State::default().progress()));
                }
                if header_deadline <= Instant::now() {
                    return Err((Error::StreamFailure, State::default().progress()));
                }
            }
        }
    };
    if header_deadline <= Instant::now() {
        return Err((Error::StreamFailure, State::default().progress()));
    }
    if !response.status().is_success() {
        let status = response.status().as_u16();
        if let Some(error) = network.proxy_response_error(&url, status) {
            return Err((Error::Outbound(error), State::default().progress()));
        }
        let body = read_capped_error_body(
            response,
            &url,
            is_canceled,
            network,
            deadlines::StreamDeadlines::new(Instant::now()),
        )
        .await
        .map_err(|error| (error, State::default().progress()))?;
        return Err((Error::Http(status, body), State::default().progress()));
    }
    let mut response = response;
    let mut state = State::default();
    let mut pending = Vec::new();
    let mut deadlines = deadlines::StreamDeadlines::new(Instant::now());
    loop {
        if is_canceled() {
            return Err((Error::Canceled, state.progress()));
        }
        let mut next_chunk = Box::pin(response.chunk());
        let chunk = loop {
            tokio::select! {
                chunk = &mut next_chunk => break chunk.map_err(|error| {
                    (Error::Outbound(network.reqwest_error(
                        &url, tau_provider::OutboundPhase::Body, &error,
                    )), state.progress())
                })?,
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadlines.next_deadline())) => {
                    return Err((Error::StreamFailure, state.progress()));
                }
                () = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {
                    if is_canceled() {
                        return Err((Error::Canceled, state.progress()));
                    }
                    if deadlines.expired(Instant::now()) {
                        return Err((Error::StreamFailure, state.progress()));
                    }
                }
            }
        };
        let Some(chunk) = chunk else {
            return Ok(state);
        };
        if deadlines.expired(Instant::now()) {
            return Err((Error::StreamFailure, state.progress()));
        }
        state.bytes = state.bytes.saturating_add(chunk.len() as u64);
        if MAX_RESPONSE_BYTES < state.bytes {
            return Err((Error::StreamFailure, state.progress()));
        }
        pending.extend_from_slice(&chunk);
        if MAX_EVENT_BYTES < pending.len() {
            return Err((Error::StreamFailure, state.progress()));
        }
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line = pending.drain(..=newline).collect::<Vec<_>>();
            while matches!(line.last(), Some(b'\r' | b'\n')) {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if MAX_EVENT_BYTES < line.len() {
                return Err((Error::StreamFailure, state.progress()));
            }
            if let Some(data) = line.strip_prefix("data:").map(str::trim_start) {
                if data == "[DONE]" {
                    state.terminal = true;
                    return Ok(state);
                }
                let qualifying_progress = state
                    .apply_event(data)
                    .map_err(|error| (error, state.progress()))?;
                on_update(state.progress());
                if qualifying_progress {
                    deadlines.renew_for_qualifying_progress(Instant::now());
                }
                if state.terminal {
                    return Ok(state);
                }
            }
        }
    }
}

async fn read_capped_error_body(
    mut response: reqwest::Response,
    url: &str,
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
    deadlines: deadlines::StreamDeadlines,
) -> Result<String, Error> {
    let mut body = Vec::new();
    while body.len() < MAX_HTTP_ERROR_BODY_BYTES as usize {
        let mut next_chunk = Box::pin(response.chunk());
        let chunk = loop {
            tokio::select! {
                chunk = &mut next_chunk => break chunk.map_err(|error| Error::Outbound(
                    network.reqwest_error(url, tau_provider::OutboundPhase::Body, &error)
                ))?,
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadlines.next_deadline())) => {
                    return Err(Error::StreamFailure);
                }
                () = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {
                    if is_canceled() {
                        return Err(Error::Canceled);
                    }
                    if deadlines.expired(Instant::now()) {
                        return Err(Error::StreamFailure);
                    }
                }
            }
        };
        let Some(chunk) = chunk else { break };
        let remaining = (MAX_HTTP_ERROR_BODY_BYTES as usize).saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn output_index(event: &Value) -> Result<u32, Error> {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .ok_or(Error::UnsupportedOutput)
}

fn reasoning_item_id(item: &Value) -> Result<Option<ReasoningItemId>, Error> {
    item.get("id")
        .map(|id| {
            id.as_str()
                .map(|id| ReasoningItemId(id.to_owned()))
                .ok_or(Error::UnsupportedOutput)
        })
        .transpose()
}

fn reasoning_content_index(event: &Value) -> Result<ReasoningContentIndex, Error> {
    event
        .get("content_index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .map(ReasoningContentIndex)
        .ok_or(Error::UnsupportedOutput)
}

fn reasoning_slots_agree(streamed: &Slot, terminal: &Slot) -> bool {
    if terminal.state != SlotState::ReasoningCompleted {
        return false;
    }
    if streamed.reasoning_item_id.is_some()
        && streamed.reasoning_item_id != terminal.reasoning_item_id
    {
        return false;
    }
    if streamed.reasoning_text.is_some() && streamed.reasoning_text != terminal.reasoning_text {
        return false;
    }
    streamed.state != SlotState::ReasoningCompleted || streamed.item == terminal.item
}

fn plain_reasoning(item: &Value, phase: OutputItemPhase) -> Result<PlainReasoning, Error> {
    if item.get("encrypted_content").is_some()
        || item
            .get("summary")
            .is_some_and(|summary| !matches!(summary, Value::Array(parts) if parts.is_empty()))
    {
        return Err(Error::UnsupportedOutput);
    }
    let Some(content) = item.get("content") else {
        return matches!(phase, OutputItemPhase::Added)
            .then_some(PlainReasoning {
                parts: BTreeMap::new(),
                display: None,
            })
            .ok_or(Error::UnsupportedOutput);
    };
    let parts = content.as_array().ok_or(Error::UnsupportedOutput)?;
    if !matches!(phase, OutputItemPhase::Added) && parts.is_empty() {
        return Err(Error::UnsupportedOutput);
    }
    let mut reasoning_parts = BTreeMap::new();
    for (index, part) in parts.iter().enumerate() {
        if part["type"].as_str() != Some("reasoning_text") {
            return Err(Error::UnsupportedOutput);
        }
        reasoning_parts.insert(
            ReasoningContentIndex(u32::try_from(index).map_err(|_| Error::UnsupportedOutput)?),
            part["text"]
                .as_str()
                .ok_or(Error::UnsupportedOutput)?
                .to_owned(),
        );
    }
    let text = reasoning_parts.values().cloned().collect::<String>();
    Ok(PlainReasoning {
        parts: reasoning_parts,
        display: (!text.is_empty()).then_some(ReasoningTextItem {
            kind: ReasoningTextKind::Full,
            text,
        }),
    })
}

fn tool_call_extra_fields(item: &Value) -> Option<tau_proto::CborValue> {
    let object = item.as_object()?;
    let extras = object
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "type" | "id" | "status" | "call_id" | "name" | "arguments" | "input"
            )
        })
        .map(|(key, value)| {
            (
                tau_proto::CborValue::Text(key.clone()),
                tau_proto::json_to_cbor(value),
            )
        })
        .collect::<Vec<_>>();
    (!extras.is_empty()).then_some(tau_proto::CborValue::Map(extras))
}

fn message_item(text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    })
}

fn append_text(message: &mut MessageItem, text: &str) {
    match message.content.last_mut() {
        Some(ContentPart::Text { text: existing })
        | Some(ContentPart::HarnessInternalText { text: existing }) => existing.push_str(text),
        None => message.content.push(ContentPart::Text {
            text: text.to_owned(),
        }),
    }
}

fn parse_usage(value: Option<&Value>) -> Option<ProviderTokenUsage> {
    let value = value?;
    let input = value["input_tokens"].as_u64()?;
    let reads = value
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64);
    let writes = value
        .pointer("/input_tokens_details/cache_write_tokens")
        .and_then(Value::as_u64)
        .or_else(|| value["cache_write_tokens"].as_u64());
    let cache = (reads.is_some() || writes.is_some()).then(|| {
        Box::new(
            tau_proto::ProviderCacheUsage {
                read_tokens: reads,
                write_tokens: writes,
                miss_tokens: None,
                cacheable_prefix_tokens: None,
                refresh_reason: Some(tau_proto::ProviderCacheRefreshReason::OrdinaryRequest),
                expiry_confidence: Some(tau_proto::ProviderCacheExpiryConfidence::Unknown),
                avoided_prefill_tokens: reads,
                storage_token_micros: None,
            }
            .normalized(input),
        )
    });
    Some(ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: input,
        prompt_cached_tokens: cache
            .as_deref()
            .and_then(|cache| cache.read_tokens)
            .unwrap_or(0),
        prompt_cache_read_ceiling_tokens: None,
        cache,
        response_received_tokens: value["output_tokens"].as_u64()?,
        stats: Default::default(),
    })
}

/// Serializable public Responses request with raw replay-capable input items.
#[derive(Serialize)]
struct RequestBody {
    /// Upstream model identifier.
    model: String,
    /// Complete typed transcript for stateless replay.
    input: Vec<ResponsesInputItem>,
    /// Public Responses attempts always request SSE output.
    stream: bool,
    /// Harness-selected reasoning effort lowered to the public Responses shape.
    reasoning: Reasoning,
    /// Optional provider instructions.
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    /// Stable agent-derived cache key for an explicitly cache-capable route.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    /// Legacy automatic-cache retention for an explicitly cache-capable route.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<&'static str>,
    /// Explicit OpenAI cache policy for a route with one typed breakpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_options: Option<PromptCacheOptions>,
    /// Optional output-token limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    /// Function tool definitions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    /// Optional closed tool selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

/// Public Responses reasoning options for one request.
#[derive(Serialize)]
struct Reasoning {
    /// Upstream spelling for the harness-effective reasoning effort.
    effort: &'static str,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ResponsesInputItem {
    /// Exact provider JSON retained for cache-stable replay.
    Raw(Box<RawValue>),
    /// Semantically constructed input item.
    Json(Value),
    /// Tau-constructed non-assistant message eligible for a cache breakpoint.
    TauInputMessage(Value),
}

/// Exact explicit cache policy accepted by the public Responses wire API.
#[derive(Serialize)]
struct PromptCacheOptions {
    /// Explicit mode suppresses OpenAI's implicit cache breakpoint.
    mode: &'static str,
    /// OpenAI's currently supported explicit cache lifetime.
    ttl: &'static str,
}

fn build_request(
    prompt: &tau_proto::AgentPromptCreated,
    config: &AttemptConfig,
    model: &AttemptModel,
) -> Result<RequestBody, Error> {
    let mut input = prompt
        .context
        .flatten_iter()
        .map(|item| lower_item(&item))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let tools = prompt
        .tools
        .iter()
        .map(lower_tool)
        .collect::<Result<Vec<_>, _>>()?;
    let tool_choice = match (prompt.tool_choice, prompt.tools.is_empty()) {
        (ToolChoice::None, _) => Some("none".to_owned()),
        (ToolChoice::Auto, false) => Some("auto".to_owned()),
        _ => None,
    };
    let explicit_cache = matches!(config.prompt_cache, Some(PromptCachePolicy::Explicit));
    if explicit_cache && !mark_first_input_text(&mut input) {
        return Err(Error::InvalidRequest);
    }
    let legacy_retention = match config.prompt_cache {
        Some(PromptCachePolicy::Legacy(retention)) => Some(retention.wire()),
        Some(PromptCachePolicy::Explicit) | None => None,
    };
    Ok(RequestBody {
        model: model.id.as_str().to_owned(),
        input,
        stream: true,
        reasoning: Reasoning {
            effort: effort_wire(prompt.model_params.effort),
        },
        instructions: (!prompt.system_prompt.trim().is_empty())
            .then(|| prompt.system_prompt.clone()),
        prompt_cache_key: config
            .prompt_cache
            .is_some()
            .then(|| format!("tau:{}", prompt.agent_id)),
        prompt_cache_retention: legacy_retention,
        prompt_cache_options: explicit_cache.then_some(PromptCacheOptions {
            mode: "explicit",
            ttl: "30m",
        }),
        max_output_tokens: (config.max_output_tokens != 0).then_some(config.max_output_tokens),
        tools,
        tool_choice,
    })
}

fn effort_wire(effort: Effort) -> &'static str {
    match effort {
        Effort::Off => "none",
        Effort::Minimal => "minimal",
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

fn lower_item(item: &ContextItem) -> Result<Option<ResponsesInputItem>, Error> {
    match item {
        ContextItem::Message(message) => {
            if message.role == ContextRole::Assistant
                && let Some(raw) = &message.responses_raw_json
            {
                let mut value =
                    serde_json::from_str::<Value>(raw).map_err(|_| Error::UnsupportedOutput)?;
                if !is_text_assistant_message(&value) {
                    return Err(Error::UnsupportedOutput);
                }
                rebase_assistant_message(&mut value, message);
                return Ok(Some(ResponsesInputItem::Json(value)));
            }
            let role = match message.role {
                ContextRole::System => "system",
                ContextRole::Developer => "developer",
                ContextRole::User => "user",
                ContextRole::Assistant => "assistant",
            };
            let text = message
                .content
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } | ContentPart::HarnessInternalText { text } => {
                        text.as_str()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let part_type = if message.role == ContextRole::Assistant {
                "output_text"
            } else {
                "input_text"
            };
            Ok((!text.is_empty()).then(|| {
                let value = serde_json::json!({
                    "role": role,
                    "content": [{"type": part_type, "text": text}],
                });
                if message.role == ContextRole::Assistant {
                    ResponsesInputItem::Json(value)
                } else {
                    ResponsesInputItem::TauInputMessage(value)
                }
            }))
        }
        ContextItem::ToolCall(call) if call.tool_type == ToolType::Function => {
            Ok(Some(ResponsesInputItem::Json(lower_call(call))))
        }
        ContextItem::ToolResult(result) if result.tool_type == ToolType::Function => {
            Ok(Some(ResponsesInputItem::Json(serde_json::json!({
                "type": "function_call_output",
                "call_id": result.call_id,
                "output": render_tool_result(result),
            }))))
        }
        ContextItem::ToolCall(_) | ContextItem::ToolResult(_) => Err(Error::UnsupportedTool),
        ContextItem::Reasoning(item) => {
            let value = item
                .raw_json
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()
                .map_err(|_| Error::UnsupportedOutput)?
                .unwrap_or_else(|| cbor_to_json(&item.value));
            plain_reasoning(&value, OutputItemPhase::Completed)?;
            match &item.raw_json {
                Some(raw) => RawValue::from_string(raw.clone())
                    .map(ResponsesInputItem::Raw)
                    .map(Some)
                    .map_err(|_| Error::UnsupportedOutput),
                None => Ok(Some(ResponsesInputItem::Json(value))),
            }
        }
        ContextItem::UnknownProviderItem(_) => Err(Error::UnsupportedOutput),
        ContextItem::ReasoningText(_) => Ok(None),
        ContextItem::CompactionTrigger | ContextItem::Compaction(_) => {
            Err(Error::UnsupportedOutput)
        }
    }
}

fn mark_first_input_text(input: &mut [ResponsesInputItem]) -> bool {
    for item in input {
        let ResponsesInputItem::TauInputMessage(message) = item else {
            continue;
        };
        let Some(content) = message["content"].as_array_mut() else {
            continue;
        };
        let Some(part) = content.iter_mut().find(|part| {
            part["type"] == "input_text"
                && part["text"].as_str().is_some_and(|text| !text.is_empty())
        }) else {
            continue;
        };
        let Some(object) = part.as_object_mut() else {
            continue;
        };
        object.insert(
            "prompt_cache_breakpoint".to_owned(),
            serde_json::json!({"mode": "explicit"}),
        );
        return true;
    }
    false
}

fn is_text_assistant_message(value: &Value) -> bool {
    value["type"] == "message"
        && value["role"] == "assistant"
        && value["content"].as_array().is_some_and(|parts| {
            parts.iter().all(|part| {
                matches!(part["type"].as_str(), Some("output_text") | Some("text"))
                    && part["text"].is_string()
            })
        })
}

fn rebase_assistant_message(value: &mut Value, message: &MessageItem) {
    let text = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } | ContentPart::HarnessInternalText { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    value["content"] = Value::Array(vec![
        serde_json::json!({"type": "output_text", "text": text, "annotations": []}),
    ]);
}

fn lower_call(call: &ToolCallItem) -> Value {
    let arguments = call.raw_arguments_json.clone().unwrap_or_else(|| {
        serde_json::to_string(&cbor_to_json(&call.arguments)).unwrap_or_else(|_| "{}".to_owned())
    });
    let mut value = serde_json::json!({
        "type": "function_call",
        "call_id": call.call_id,
        "name": call.name,
        "arguments": arguments,
    });
    if let Some(envelope) = &call.responses_envelope {
        if let Some(id) = &envelope.item_id {
            value["id"] = Value::String(id.clone());
        }
        if let Some(status) = &envelope.status {
            value["status"] = Value::String(status.clone());
        }
        if let Some(tau_proto::CborValue::Map(fields)) = &envelope.extra_fields
            && let Value::Object(object) = &mut value
        {
            for (key, value) in fields {
                if let tau_proto::CborValue::Text(key) = key {
                    object
                        .entry(key.clone())
                        .or_insert_with(|| cbor_to_json(value));
                }
            }
        }
    }
    value
}

fn lower_tool(tool: &ToolDefinition) -> Result<Value, Error> {
    if tool.tool_type != ToolType::Function {
        return Err(Error::UnsupportedTool);
    }
    Ok(serde_json::json!({
        "type": "function",
        "name": tool.model_visible_name.as_ref().unwrap_or(&tool.name),
        "description": tool.description,
        "parameters": tool.parameters,
    }))
}

fn render_tool_result(result: &tau_proto::ToolResultItem) -> String {
    match &result.status {
        ToolResultStatus::Success => result.output.render(),
        ToolResultStatus::Error { message } => {
            let mut output = result.output.clone();
            output.headers.insert(
                0,
                ToolResponseHeader {
                    key: "error".to_owned(),
                    value: message.clone(),
                },
            );
            output.render()
        }
        ToolResultStatus::Cancelled { reason } => tau_proto::ToolResponse {
            raw: tau_proto::CborValue::Null,
            headers: vec![ToolResponseHeader {
                key: "cancelled".to_owned(),
                value: reason.clone(),
            }],
            body: String::new(),
        }
        .render(),
    }
}

fn cbor_to_json(value: &tau_proto::CborValue) -> Value {
    match value {
        tau_proto::CborValue::Null => Value::Null,
        tau_proto::CborValue::Bool(value) => Value::Bool(*value),
        tau_proto::CborValue::Integer(value) => Value::Number(
            serde_json::Number::from_i128((*value).into()).unwrap_or_else(|| 0.into()),
        ),
        tau_proto::CborValue::Float(value) => serde_json::Number::from_f64(*value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        tau_proto::CborValue::Text(value) => Value::String(value.clone()),
        tau_proto::CborValue::Bytes(value) => Value::Array(
            value
                .iter()
                .map(|byte| Value::Number((*byte).into()))
                .collect(),
        ),
        tau_proto::CborValue::Array(items) => {
            Value::Array(items.iter().map(cbor_to_json).collect())
        }
        tau_proto::CborValue::Map(items) => Value::Object(
            items
                .iter()
                .map(|(key, value)| {
                    let key = match key {
                        tau_proto::CborValue::Text(value) => value.clone(),
                        _ => serde_json::to_string(&cbor_to_json(key)).unwrap_or_default(),
                    };
                    (key, cbor_to_json(value))
                })
                .collect(),
        ),
        tau_proto::CborValue::Tag(_, value) => cbor_to_json(value),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod deadlines_tests;
#[cfg(test)]
mod tests;
