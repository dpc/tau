//! Generic public API-key HTTP/SSE Responses backend.
//!
//! This intentionally does not share the private ChatGPT/Codex WebSocket
//! implementation.  It sends a complete typed transcript on every turn.

use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tau_proto::{
    ContentPart, ContextItem, ContextRole, MessageItem, ModelName, ProviderStopReason,
    ProviderTokenUsage, ResponsesToolCallEnvelope, ToolCallItem, ToolChoice, ToolDefinition,
    ToolResponseHeader, ToolResultStatus, ToolType,
};
use tau_provider::retry_policy::{
    RetryClass, RetryDecision, classify_error_code, parse_json_error_code,
};
use tokio::runtime as path_tokio_runtime;

const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HTTP_ERROR_BODY_BYTES: u64 = 64 * 1024;
const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const ATTEMPT_PHASE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Resolved endpoint configuration for one public Responses attempt.
#[derive(Clone)]
pub struct AttemptConfig {
    /// Base URL without the `/responses` suffix.
    pub base_url: String,
    /// Optional bearer credential.
    pub api_key: String,
    /// Requested output token cap, or zero to omit it.
    pub max_output_tokens: u32,
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
    /// Safe fixed diagnostic.
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
            Self::InvalidRequest | Self::UnsupportedTool | Self::UnsupportedOutput => {
                Some(tau_proto::ProviderFailureKind::RequestRejected)
            }
            _ => None,
        }
    }
}

fn failure_kind(status: u16, body: &str) -> Option<tau_proto::ProviderFailureKind> {
    if parse_json_error_code(body).as_deref() == Some("context_length_exceeded") {
        return Some(tau_proto::ProviderFailureKind::ContextWindowExceeded);
    }
    matches!(status, 400 | 404 | 409 | 413 | 422)
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
            if state.items.is_empty() {
                terminal(Error::EmptyResponse, progress)
            } else {
                let output_items = state
                    .items
                    .into_iter()
                    .map(|slot| slot.item)
                    .collect::<Vec<_>>();
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
                "Responses supports text and Function output only".to_owned()
            }
            Error::InvalidRequest => "Responses request was invalid".to_owned(),
            Error::Http(status, _) => format!("provider returned HTTP {status}"),
            _ => "Responses request failed".to_owned(),
        },
        failure_kind: error.failure_kind(),
        stop_reason: ProviderStopReason::EndTurn,
        progress,
    })
}

#[derive(Debug)]
struct Slot {
    index: u32,
    item: ContextItem,
}

#[derive(Debug, Default)]
struct State {
    items: Vec<Slot>,
    bytes: u64,
    terminal: bool,
    usage: Option<ProviderTokenUsage>,
    response_id: Option<String>,
}

impl State {
    fn progress(&self) -> AttemptProgress {
        AttemptProgress {
            output_items: self
                .items
                .iter()
                .map(|slot| AttemptOutputItem {
                    output_index: slot.index,
                    item: slot.item.clone(),
                })
                .collect(),
            response_bytes_received: self.bytes,
            has_timed_semantic_output: self.items.iter().any(|slot| match &slot.item {
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
                _ => false,
            }),
        }
    }

    fn slot_mut(&mut self, index: u32) -> &mut Slot {
        if let Some(position) = self.items.iter().position(|slot| slot.index == index) {
            return &mut self.items[position];
        }
        self.items.push(Slot {
            index,
            item: ContextItem::UnknownProviderItem(tau_proto::OpaqueProviderItem {
                value: tau_proto::CborValue::Null,
                raw_json: None,
            }),
        });
        self.items.last_mut().expect("just appended slot")
    }
}

async fn stream(
    _prompt: &tau_proto::AgentPromptCreated,
    config: &AttemptConfig,
    body: &Value,
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
    let header_deadline = Instant::now() + ATTEMPT_PHASE_TIMEOUT;
    let mut send = Box::pin(request.send());
    let response = loop {
        tokio::select! {
            response = &mut send => break response.map_err(|error| {
                (Error::Outbound(network.reqwest_error(
                    &url, tau_provider::OutboundPhase::Request, &error,
                )), State::default().progress())
            })?,
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
    if !response.status().is_success() {
        let status = response.status().as_u16();
        if let Some(error) = network.proxy_response_error(&url, status) {
            return Err((Error::Outbound(error), State::default().progress()));
        }
        let body = read_capped_error_body(response, &url, is_canceled, network)
            .await
            .map_err(|error| (error, State::default().progress()))?;
        return Err((Error::Http(status, body), State::default().progress()));
    }
    let mut response = response;
    let mut state = State::default();
    let mut pending = Vec::new();
    loop {
        if is_canceled() {
            return Err((Error::Canceled, state.progress()));
        }
        let mut next_chunk = Box::pin(response.chunk());
        let idle_deadline = Instant::now() + STREAM_IDLE_TIMEOUT;
        let chunk = loop {
            tokio::select! {
                chunk = &mut next_chunk => break chunk.map_err(|error| {
                    (Error::Outbound(network.reqwest_error(
                        &url, tau_provider::OutboundPhase::Body, &error,
                    )), state.progress())
                })?,
                () = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {
                    if is_canceled() {
                        return Err((Error::Canceled, state.progress()));
                    }
                    if idle_deadline <= Instant::now() {
                        return Err((Error::StreamFailure, state.progress()));
                    }
                }
            }
        };
        let Some(chunk) = chunk else {
            return Ok(state);
        };
        state.bytes = state.bytes.saturating_add(chunk.len() as u64);
        if MAX_RESPONSE_BYTES < state.bytes {
            return Err((Error::StreamFailure, state.progress()));
        }
        pending.extend_from_slice(&chunk);
        if MAX_SSE_LINE_BYTES < pending.len() {
            return Err((Error::StreamFailure, state.progress()));
        }
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line = pending.drain(..=newline).collect::<Vec<_>>();
            while matches!(line.last(), Some(b'\r' | b'\n')) {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if MAX_SSE_LINE_BYTES < line.len() {
                return Err((Error::StreamFailure, state.progress()));
            }
            if let Some(data) = line.strip_prefix("data:").map(str::trim_start) {
                if data == "[DONE]" {
                    state.terminal = true;
                    return Ok(state);
                }
                apply_event(&mut state, data).map_err(|error| (error, state.progress()))?;
                on_update(state.progress());
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
) -> Result<String, Error> {
    let mut body = Vec::new();
    while body.len() < MAX_HTTP_ERROR_BODY_BYTES as usize {
        let mut next_chunk = Box::pin(response.chunk());
        let deadline = Instant::now() + STREAM_IDLE_TIMEOUT;
        let chunk = loop {
            tokio::select! {
                chunk = &mut next_chunk => break chunk.map_err(|error| Error::Outbound(
                    network.reqwest_error(url, tau_provider::OutboundPhase::Body, &error)
                ))?,
                () = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {
                    if is_canceled() {
                        return Err(Error::Canceled);
                    }
                    if deadline <= Instant::now() {
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

fn apply_event(state: &mut State, data: &str) -> Result<(), Error> {
    let event: Value = serde_json::from_str(data).map_err(|_| Error::Json)?;
    match event["type"].as_str().unwrap_or("") {
        "response.output_item.added" | "response.output_item.done" => {
            let index = event["output_index"].as_u64().unwrap_or(0) as u32;
            if let Some(item) = event.get("item") {
                apply_item(state.slot_mut(index), item)?;
            }
        }
        "response.output_text.delta" | "response.content_part.delta" => {
            let index = event["output_index"].as_u64().unwrap_or(0) as u32;
            let delta = event["delta"].as_str().unwrap_or("");
            if !delta.is_empty() {
                let slot = state.slot_mut(index);
                match &mut slot.item {
                    ContextItem::Message(message) => append_text(message, delta),
                    _ => slot.item = message_item(delta),
                }
            }
        }
        "response.function_call_arguments.delta" => {
            let index = event["output_index"].as_u64().unwrap_or(0) as u32;
            let delta = event["delta"].as_str().unwrap_or("");
            if let ContextItem::ToolCall(call) = &mut state.slot_mut(index).item {
                call.raw_arguments_json
                    .get_or_insert_with(String::new)
                    .push_str(delta);
            }
        }
        "response.function_call_arguments.done" => {
            let index = event["output_index"].as_u64().unwrap_or(0) as u32;
            if let ContextItem::ToolCall(call) = &mut state.slot_mut(index).item
                && let Some(arguments) = event["arguments"].as_str()
            {
                call.raw_arguments_json = Some(arguments.to_owned());
                call.arguments = tau_proto::json_to_cbor(
                    &serde_json::from_str::<Value>(arguments).map_err(|_| Error::Json)?,
                );
            }
        }
        "response.completed" | "response.done" => {
            state.terminal = true;
            let response = event.get("response").unwrap_or(&event);
            state.response_id = response
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            state.usage = parse_usage(response.get("usage"));
            if let Some(output) = response.get("output").and_then(Value::as_array) {
                for (index, item) in output.iter().enumerate() {
                    apply_item(state.slot_mut(index as u32), item)?;
                }
            }
        }
        "response.failed" | "response.incomplete" | "error" => return Err(Error::StreamFailure),
        _ => {}
    }
    Ok(())
}

fn apply_item(slot: &mut Slot, item: &Value) -> Result<(), Error> {
    match item["type"].as_str().unwrap_or("") {
        "message" if item["role"].as_str() == Some("assistant") => {
            if !is_text_assistant_message(item) {
                return Err(Error::UnsupportedOutput);
            }
            let mut message = MessageItem {
                role: ContextRole::Assistant,
                content: Vec::new(),
                phase: None,
                responses_raw_json: Some(item.to_string()),
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
            slot.item = ContextItem::Message(message);
        }
        "function_call" => {
            let arguments = item["arguments"].as_str().unwrap_or("{}");
            let call_id = item["call_id"].as_str().ok_or(Error::InvalidRequest)?;
            let name = item["name"].as_str().ok_or(Error::InvalidRequest)?;
            slot.item = ContextItem::ToolCall(ToolCallItem {
                call_id: tau_proto::ToolCallId::new(call_id),
                name: tau_proto::ToolName::try_new(name.to_owned()).ok_or(Error::InvalidRequest)?,
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
            });
        }
        _ => return Err(Error::UnsupportedOutput),
    }
    Ok(())
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
        Some(ContentPart::Text { text: existing }) => existing.push_str(text),
        None => message.content.push(ContentPart::Text {
            text: text.to_owned(),
        }),
    }
}

fn parse_usage(value: Option<&Value>) -> Option<ProviderTokenUsage> {
    let value = value?;
    Some(ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: value["input_tokens"].as_u64()?,
        prompt_cached_tokens: value
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        prompt_cache_read_ceiling_tokens: None,
        response_received_tokens: value["output_tokens"].as_u64()?,
        stats: Default::default(),
    })
}

fn build_request(
    prompt: &tau_proto::AgentPromptCreated,
    config: &AttemptConfig,
    model: &AttemptModel,
) -> Result<Value, Error> {
    let input = prompt
        .context
        .flatten_iter()
        .map(|item| lower_item(&item))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    let tools = prompt
        .tools
        .iter()
        .map(lower_tool)
        .collect::<Result<Vec<_>, _>>()?;
    let mut body = Map::from_iter([
        (
            "model".to_owned(),
            Value::String(model.id.as_str().to_owned()),
        ),
        ("input".to_owned(), Value::Array(input)),
        ("stream".to_owned(), Value::Bool(true)),
    ]);
    if !prompt.system_prompt.trim().is_empty() {
        body.insert(
            "instructions".to_owned(),
            Value::String(prompt.system_prompt.clone()),
        );
    }
    if config.max_output_tokens != 0 {
        body.insert(
            "max_output_tokens".to_owned(),
            Value::Number(config.max_output_tokens.into()),
        );
    }
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    match (prompt.tool_choice, prompt.tools.is_empty()) {
        (ToolChoice::None, _) => {
            body.insert("tool_choice".to_owned(), Value::String("none".to_owned()));
        }
        (ToolChoice::Auto, false) => {
            body.insert("tool_choice".to_owned(), Value::String("auto".to_owned()));
        }
        _ => {}
    }
    Ok(Value::Object(body))
}

fn lower_item(item: &ContextItem) -> Result<Option<Value>, Error> {
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
                return Ok(Some(value));
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
                    ContentPart::Text { text } => text.as_str(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            let part_type = if message.role == ContextRole::Assistant {
                "output_text"
            } else {
                "input_text"
            };
            Ok((!text.is_empty()).then(|| serde_json::json!({"role": role, "content": [{"type": part_type, "text": text}]})))
        }
        ContextItem::ToolCall(call) if call.tool_type == ToolType::Function => {
            Ok(Some(lower_call(call)))
        }
        ContextItem::ToolResult(result) if result.tool_type == ToolType::Function => {
            Ok(Some(serde_json::json!({
                "type": "function_call_output",
                "call_id": result.call_id,
                "output": render_tool_result(result),
            })))
        }
        ContextItem::ToolCall(_) | ContextItem::ToolResult(_) => Err(Error::UnsupportedTool),
        ContextItem::Reasoning(_) | ContextItem::UnknownProviderItem(_) => {
            Err(Error::UnsupportedOutput)
        }
        ContextItem::ReasoningText(_)
        | ContextItem::CompactionTrigger
        | ContextItem::Compaction(_) => Err(Error::UnsupportedOutput),
    }
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
            ContentPart::Text { text } => text.as_str(),
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
mod tests;
