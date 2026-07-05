//! ChatGPT/Codex Responses API client.
//!
//! Endpoint: `POST {base_url}/codex/responses`
//!
//! SSE streaming with `response.output_text.delta` events.
//!
//! When the resolved config advertises `supports_websocket`, the provider
//! loop routes Responses turns through the [`ws`] module
//! instead — same wire envelope, persistent connection, per-session
//! pooling. This module's HTTP+SSE path is kept as the universal
//! fallback (and as the only transport for endpoints that don't
//! support WS).

use std::borrow::Cow;
use std::io::BufRead;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tau_proto::{
    ContentPart, ContextItem, ContextRole, MessageItem, ResponsesToolCallEnvelope, ToolCallItem,
    ToolResponseHeader, ToolResultItem, ToolResultStatus,
};

use crate::common::{
    LlmError, PromptPayload, StreamState, cbor_to_json, effort_wire, json_to_cbor,
};

pub mod pool;
pub mod ws;
pub mod ws_runtime;

const PROVIDER_STREAM_CASSETTE_VERSION: u32 = 1;
/// Which ChatGPT/Codex Responses surface a model is served through.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesSurface {
    /// ChatGPT/Codex OAuth-backed `/codex/responses` endpoint.
    ChatGpt,
}

impl ResponsesSurface {
    fn responses_url(self, base_url: &str) -> String {
        let _ = self;
        let base = base_url.trim_end_matches('/');
        format!("{base}/codex/responses")
    }

    fn store_value(self) -> bool {
        let _ = self;
        false
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct ProviderStreamCassette {
    version: u32,
    request: serde_json::Value,
    stream: ProviderRawEventStream,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct ProviderRawEventStream {
    raw_events: Vec<ProviderRawEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ProviderRawEvent {
    delta_micros: u64,
    raw: String,
}

pub(super) fn record_provider_raw_event_after(
    stream: &mut ProviderRawEventStream,
    delta: Duration,
    raw: impl Into<String>,
) {
    stream.raw_events.push(ProviderRawEvent {
        delta_micros: duration_micros_u64(delta),
        raw: raw.into(),
    });
}

/// Config for the ChatGPT/Codex Responses API.
#[derive(Clone, Debug)]
pub struct ResponsesConfig {
    /// Responses API surface used for endpoint and request-body differences.
    pub surface: ResponsesSurface,
    /// Base URL for the selected surface, without the final Responses path.
    pub base_url: String,
    /// Bearer credential to send in the `Authorization` header.
    pub api_key: String,
    /// Upstream model id without the Tau provider namespace.
    pub model_id: String,
    /// Total context window for the selected upstream model, in tokens.
    pub context_window: u64,
    /// `chatgpt-account-id` header extracted from JWT.
    pub account_id: Option<String>,
    /// Whether the provider's API accepts a `reasoning.effort` field.
    pub supports_reasoning_effort: bool,
    /// Whether the provider's API accepts `reasoning.summary` and
    /// streams `response.reasoning_summary_text.*` events.
    pub supports_reasoning_summary: bool,
    /// Whether the provider's API accepts a `text.verbosity` field
    /// (OpenAI Responses on GPT-5+).
    pub supports_verbosity: bool,
    /// Whether the provider's API accepts (and the model emits) the
    /// `phase` field on assistant `message` items
    /// (`commentary` / `final_answer`). When on:
    /// 1. The Responses backend stamps `phase` on every outgoing assistant
    ///    message, defaulting to `final_answer` when the stored history doesn't
    ///    carry one (matches OpenAI's deployment-checklist guidance for
    ///    backwards compatibility).
    /// 2. The SSE parser captures `phase` off the assistant `message` item so
    ///    the harness can persist it.
    ///
    /// When off, no `phase` field is sent or parsed.
    pub supports_phase: bool,
    /// Whether the provider returns `reasoning` output items with a
    /// replayable `encrypted_content` field when the request body
    /// asks for `include: ["reasoning.encrypted_content"]`. Currently
    /// the Codex Responses backend on `gpt-5.3-codex+`. When on:
    /// 1. The request body sets `include: ["reasoning.encrypted_content"]` so
    ///    the model's reasoning output items carry the encrypted blob the
    ///    harness can replay verbatim.
    /// 2. The SSE/WS parser captures each `reasoning` output item's full JSON
    ///    on `response.output_item.done` and forwards it as an ordered
    ///    `ContextItem::Reasoning` in `ProviderResponseFinished.output_items`.
    ///
    /// When off, no `include` field is sent and reasoning items are
    /// not captured. Pi calls this "encrypted reasoning replay"; it's
    /// what keeps the model's reasoning continuity intact across a
    /// broken chain (reconnect, fork, fingerprint mismatch) without
    /// having to actually re-derive it from the visible transcript.
    pub supports_encrypted_reasoning: bool,
    /// Whether to attempt a persistent WebSocket transport for this
    /// provider instead of one-shot HTTP+SSE.
    pub supports_websocket: bool,
    /// Whether this provider supports server-side context compaction.
    pub supports_compaction: bool,
    /// Whether this provider accepts the `prompt_cache_key` field.
    /// The wire key is derived per `(base_url, agent lifetime)` and is stable
    /// across prompt originators for the same target agent.
    pub supports_prompt_cache_key: bool,
}

/// Write the exact Responses request body Tau is about to send upstream.
///
/// This records the full prompt transcript, including tool results. It never
/// writes credentials or request headers. Files are written under the session
/// debug directory:
///
/// `~/.local/state/tau/sessions/<session_id>/debug/provider-requests/`.
pub(super) fn maybe_debug_write_provider_request(
    agent_prompt_id: &str,
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    transport: tau_proto::ProviderBackendTransport,
    body: &impl Serialize,
) {
    if !request.debug_provider_requests {
        return;
    }
    if let Err(error) =
        debug_write_provider_request(agent_prompt_id, config, request, transport, body)
    {
        tracing::warn!(
            target: crate::LOG_TARGET,
            session_id = %request.session_id,
            agent_prompt_id,
            "failed to write provider request debug log: {error}",
        );
    }
}

/// Return the provider request debug directory for an explicitly durable
/// session.
///
/// This helper intentionally returns `None` unless the caller passes an
/// explicit durable-session signal and the session directory already exists.
/// Provider diagnostics must not infer persistence from filesystem shape or
/// create per-session state directories on their own, because ephemeral
/// sessions can reuse a session id with older durable state and must not gain
/// durable debug artifacts as a side effect of provider execution.
pub fn debug_provider_request_dir(
    session_id: &str,
    debug_provider_requests: bool,
) -> Option<PathBuf> {
    let state = tau_config::settings::state_dir()?;
    debug_provider_request_dir_in(&state, session_id, debug_provider_requests)
}

fn debug_provider_request_dir_in(
    state: &std::path::Path,
    session_id: &str,
    debug_provider_requests: bool,
) -> Option<PathBuf> {
    if !debug_provider_requests {
        return None;
    }
    let session_dir = tau_config::settings::sessions_dir_of(state).join(session_id);
    session_dir
        .is_dir()
        .then(|| session_dir.join("debug").join("provider-requests"))
}

fn debug_write_provider_request(
    agent_prompt_id: &str,
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    transport: tau_proto::ProviderBackendTransport,
    body: &impl Serialize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(dir) = debug_provider_request_dir(request.session_id, request.debug_provider_requests)
    else {
        return Ok(());
    };
    std::fs::create_dir_all(&dir)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let transport_label = provider_backend_transport_label(transport);
    let path = dir.join(format!(
        "{ts}-{agent_prompt_id}-{transport_label}-request.json"
    ));
    let body = serde_json::to_value(body)?;
    let metadata = serde_json::json!({
        "session_id": request.session_id,
        "agent_prompt_id": agent_prompt_id,
        "transport": transport_label,
        "backend": "responses",
        "model": config.model_id,
        "context_item_count": request.context.flatten_iter().count(),
        "tool_count": request.tools.len(),
        "tool_choice": request.tool_choice,
        "body": body,
    });
    std::fs::write(path, serde_json::to_vec_pretty(&metadata)?)?;
    Ok(())
}

/// Calls the Codex Responses API with SSE streaming.
///
/// `on_update` is invoked on each visible output change with the current
/// accumulated stream state snapshot.
///
/// HTTP+SSE requests always send the full prompt. The Codex endpoint is called
/// with `store: false`, so `previous_response_id` chaining is intentionally a
/// WebSocket-only optimization.
pub fn responses_stream(
    agent_prompt_id: &str,
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<StreamState, LlmError> {
    let body = build_request(config, request, None);
    let vcr_config = load_vcr_config()?;
    if let Some(vcr_config) = vcr_config.as_ref() {
        let request_body = serde_json::to_value(&body).map_err(LlmError::Json)?;
        if let Some(cassette) = load_provider_stream_cassette(
            vcr_config,
            request,
            agent_prompt_id,
            tau_proto::ProviderBackendTransport::HttpSse,
            &request_body,
        )? {
            return responses_stream_replay(&cassette.stream, on_update);
        }
        let store = vcr_config.store();
        let key = provider_vcr_key(
            request,
            agent_prompt_id,
            tau_proto::ProviderBackendTransport::HttpSse,
        );
        let mut stream = ProviderRawEventStream::default();
        let state = responses_stream_live(
            agent_prompt_id,
            config,
            request,
            &body,
            Some(&mut stream),
            on_update,
        )?;
        let cassette = ProviderStreamCassette {
            version: PROVIDER_STREAM_CASSETTE_VERSION,
            request: request_body,
            stream,
        };
        store.put(&key, &cassette).map_err(LlmError::Vcr)?;
        return Ok(state);
    }
    responses_stream_live(agent_prompt_id, config, request, &body, None, on_update)
}

fn responses_stream_live(
    agent_prompt_id: &str,
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    body: &ResponsesRequest,
    mut recording_stream: Option<&mut ProviderRawEventStream>,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<StreamState, LlmError> {
    maybe_debug_write_provider_request(
        agent_prompt_id,
        config,
        request,
        tau_proto::ProviderBackendTransport::HttpSse,
        body,
    );
    let url = config.surface.responses_url(&config.base_url);
    let body_str = serde_json::to_string(body).map_err(LlmError::Json)?;

    let mut req = tau_provider::oauth::proxy_agent()
        .post(&url)
        .content_type("application/json")
        .header("Accept", "text/event-stream")
        .header("Authorization", format!("Bearer {}", config.api_key))
        .header("OpenAI-Beta", "responses=experimental");

    if let Some(ref account_id) = config.account_id {
        req = req.header("chatgpt-account-id", account_id);
    }

    let mut response = req
        .send(&body_str)
        .map_err(|e| LlmError::Http(Box::new(e)))?;
    if !response.status().is_success() {
        let code = response.status().as_u16();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        return Err(LlmError::HttpStatus(code, body));
    }

    let reader = std::io::BufReader::new(response.body_mut().as_reader());
    let mut state = StreamState::new();
    let mut recording_last_event_at = Instant::now();
    for line in reader.lines() {
        let line = line.map_err(LlmError::Io)?;
        if !line.starts_with("data: ") {
            continue;
        }
        let data = line
            .strip_prefix("data: ")
            .expect("line starts with data prefix");
        if let Some(stream) = recording_stream.as_deref_mut() {
            let now = Instant::now();
            let delta = now.saturating_duration_since(recording_last_event_at);
            recording_last_event_at = now;
            record_provider_raw_event_after(stream, delta, format!("{line}\n\n"));
        }
        if apply_sse_data(&mut state, data, on_update)? {
            break;
        }
    }

    Ok(state)
}

fn responses_stream_replay(
    stream: &ProviderRawEventStream,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<StreamState, LlmError> {
    let mut state = StreamState::new();
    for event in &stream.raw_events {
        std::thread::sleep(scale_delay(
            Duration::from_micros(event.delta_micros),
            100.0,
        ));
        if apply_raw_sse_event(&mut state, &event.raw, on_update)? {
            break;
        }
    }
    Ok(state)
}

fn apply_raw_sse_event(
    state: &mut StreamState,
    raw: &str,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<bool, LlmError> {
    let Some(data) = raw.lines().find_map(|line| line.strip_prefix("data: ")) else {
        return Ok(false);
    };
    apply_sse_data(state, data, on_update)
}

fn apply_sse_data(
    state: &mut StreamState,
    data: &str,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<bool, LlmError> {
    let data = data.trim_end();
    if data == "[DONE]" {
        return Ok(true);
    }
    apply_raw_json_event(state, data, on_update)
}

fn load_vcr_config() -> Result<Option<tau_vcr::VcrConfig>, LlmError> {
    Ok(tau_vcr::VcrConfig::from_env())
}

pub(super) fn load_provider_stream_cassette(
    vcr_config: &tau_vcr::VcrConfig,
    request: &PromptPayload<'_>,
    agent_prompt_id: &str,
    transport: tau_proto::ProviderBackendTransport,
    request_body: &serde_json::Value,
) -> Result<Option<ProviderStreamCassette>, LlmError> {
    let store = vcr_config.store();
    let key = provider_vcr_key(request, agent_prompt_id, transport);
    let cassette = store
        .get::<ProviderStreamCassette>(&key)
        .map_err(LlmError::Vcr)?;
    match (vcr_config.mode, cassette) {
        (tau_vcr::VcrMode::Off, _) => Ok(None),
        (tau_vcr::VcrMode::ReplayOnly, None) => {
            Err(LlmError::Vcr(tau_vcr::VcrError::Missing { key }))
        }
        (tau_vcr::VcrMode::ReplayOnly | tau_vcr::VcrMode::RecordIfMissing, Some(cassette)) => {
            validate_provider_stream_cassette(&key, &cassette, request_body)?;
            Ok(Some(cassette))
        }
        (tau_vcr::VcrMode::RecordIfMissing, None) => Ok(None),
    }
}

pub(super) fn provider_vcr_key(
    request: &PromptPayload<'_>,
    agent_prompt_id: &str,
    transport: tau_proto::ProviderBackendTransport,
) -> String {
    format!(
        "{}-{}-{}",
        request.session_id.as_str(),
        agent_prompt_id,
        provider_backend_transport_label(transport)
    )
}

fn validate_provider_stream_cassette(
    key: &str,
    cassette: &ProviderStreamCassette,
    request_body: &serde_json::Value,
) -> Result<(), LlmError> {
    if cassette.version != PROVIDER_STREAM_CASSETTE_VERSION {
        return Err(LlmError::Vcr(tau_vcr::VcrError::UnsupportedVersion {
            key: key.to_owned(),
            version: cassette.version,
        }));
    }
    if cassette.request != *request_body {
        return Err(LlmError::Vcr(tau_vcr::request_mismatch(
            key,
            &cassette.request,
            request_body,
        )));
    }
    Ok(())
}

fn provider_backend_transport_label(
    transport: tau_proto::ProviderBackendTransport,
) -> &'static str {
    match transport {
        tau_proto::ProviderBackendTransport::HttpSse => "http-sse",
        tau_proto::ProviderBackendTransport::Websocket => "websocket",
    }
}

/// Applies one decoded `response.*` event when raw JSON is unavailable.
///
/// This compatibility path preserves semantic stream handling but cannot keep
/// raw provider item JSON for replay sidecars. HTTP+SSE, WebSocket, and VCR
/// replay should call the raw JSON event helper with the original event text so
/// opaque provider items and assistant message items can preserve provider
/// envelope/content metadata.
pub fn apply_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<bool, LlmError> {
    apply_event_with_raw_item(state, event, None, on_update)
}

/// Applies one raw upstream Responses event while preserving replay sidecars.
///
/// Returns `Ok(true)` when the event terminates the stream
/// (`response.completed` / `response.done`), `Ok(false)` to keep reading, or an
/// error when the server signaled a model-side failure that should be surfaced
/// as [`LlmError`].
pub(super) fn apply_raw_json_event(
    state: &mut StreamState,
    data: &str,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<bool, LlmError> {
    let event: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    apply_event_with_raw_item(state, &event, raw_output_item_json(data), on_update)
}

fn apply_event_with_raw_item(
    state: &mut StreamState,
    event: &serde_json::Value,
    raw_item_json: Option<&str>,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<bool, LlmError> {
    let event_type = event["type"].as_str().unwrap_or("");

    let stream_update_applied =
        apply_stream_update_event(state, event, raw_item_json, event_type, on_update)?;
    if stream_update_applied {
        return Ok(false);
    }

    match event_type {
        "response.completed" | "response.done" => {
            apply_terminal_event(state, event);
            Ok(true)
        }
        "response.incomplete" => Err(response_incomplete_error(event)),
        "response.failed" => Err(response_failed_error(event)),
        "error" => Err(stream_error_event(event)),
        _ => Ok(false),
    }
}

fn apply_stream_update_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    raw_item_json: Option<&str>,
    event_type: &str,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<bool, LlmError> {
    match event_type {
        "response.output_text.delta" => apply_output_text_delta_event(state, event, on_update)?,
        "response.output_text.done" => apply_output_text_done_event(state, event, on_update)?,
        "response.reasoning_summary_text.delta" => {
            apply_reasoning_summary_delta_event(state, event, on_update)?;
        }
        "response.reasoning_summary_part.added" => apply_reasoning_summary_part_added(state, event),
        "response.function_call_arguments.delta" => {
            apply_tool_delta_event(state, event, tau_proto::ToolType::Function, on_update)?;
        }
        "response.function_call_arguments.done" => apply_tool_done_event(
            state,
            event,
            tau_proto::ToolType::Function,
            ToolInputField::Arguments,
            on_update,
        )?,
        "response.custom_tool_call_input.delta" => {
            apply_tool_delta_event(state, event, tau_proto::ToolType::Custom, on_update)?;
        }
        "response.custom_tool_call_input.done" => apply_tool_done_event(
            state,
            event,
            tau_proto::ToolType::Custom,
            ToolInputField::Input,
            on_update,
        )?,
        "response.output_item.added" => {
            apply_output_item_event(
                state,
                event,
                raw_item_json,
                OutputItemEventKind::Added,
                on_update,
            )?;
        }
        "response.output_item.done" => {
            apply_output_item_event(
                state,
                event,
                raw_item_json,
                OutputItemEventKind::Done,
                on_update,
            )?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn raw_output_item_json(event_json: &str) -> Option<&str> {
    #[derive(Deserialize)]
    struct RawOutputItemEvent<'a> {
        #[serde(borrow)]
        item: Option<&'a RawValue>,
    }

    serde_json::from_str::<RawOutputItemEvent<'_>>(event_json)
        .ok()
        .and_then(|event| event.item.map(RawValue::get))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputItemEventKind {
    Added,
    Done,
}

impl OutputItemEventKind {
    fn is_done(self) -> bool {
        matches!(self, Self::Done)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolInputField {
    Arguments,
    Input,
}

fn event_output_index(event: &serde_json::Value) -> usize {
    event["output_index"].as_u64().unwrap_or(0) as usize
}

fn apply_output_text_delta_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(), LlmError> {
    let Some(delta) = event["delta"].as_str() else {
        return Ok(());
    };
    let output_index = event_output_index(event);
    state.check_message_delta(output_index, delta)?;
    state.append_message_delta_at(output_index, delta);
    on_update(state);
    Ok(())
}

fn apply_output_text_done_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(), LlmError> {
    let Some(text) = event["text"].as_str() else {
        return Ok(());
    };
    let output_index = event_output_index(event);
    state.check_message_snapshot(output_index, text)?;
    state.set_message_text_at(output_index, text);
    on_update(state);
    Ok(())
}

fn apply_reasoning_summary_delta_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(), LlmError> {
    let Some(delta) = event["delta"].as_str() else {
        return Ok(());
    };
    let output_index = event_output_index(event);
    state.check_reasoning_delta(output_index, delta)?;
    state.append_reasoning_summary_delta_at(output_index, delta);
    on_update(state);
    Ok(())
}

fn apply_reasoning_summary_part_added(state: &mut StreamState, event: &serde_json::Value) {
    // Each summary part is a separate paragraph. Insert a blank line between
    // parts so consecutive paragraphs are visually separated.
    let output_index = event_output_index(event);
    state.start_reasoning_summary_part_at(output_index);
}

fn apply_tool_delta_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    tool_type: tau_proto::ToolType,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(), LlmError> {
    let Some(delta) = event["delta"].as_str() else {
        return Ok(());
    };
    let output_index = event_output_index(event);
    check_tool_delta(state, output_index, tool_type, delta)?;
    state
        .tool_call_at_mut(output_index, tool_type)
        .arguments_json
        .push_str(delta);
    on_update(state);
    Ok(())
}

fn apply_tool_done_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    tool_type: tau_proto::ToolType,
    input_field: ToolInputField,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(), LlmError> {
    let Some(input) = tool_input_from_event(event, input_field) else {
        return Ok(());
    };
    let output_index = event_output_index(event);
    check_tool_snapshot(state, output_index, tool_type, input)?;
    state
        .tool_call_at_mut(output_index, tool_type)
        .arguments_json = input.to_owned();
    on_update(state);
    Ok(())
}

fn tool_input_from_event(event: &serde_json::Value, input_field: ToolInputField) -> Option<&str> {
    match input_field {
        ToolInputField::Arguments => event["arguments"].as_str(),
        ToolInputField::Input => event["input"].as_str(),
    }
}

fn check_tool_delta(
    state: &mut StreamState,
    output_index: usize,
    tool_type: tau_proto::ToolType,
    delta: &str,
) -> Result<(), LlmError> {
    match tool_type {
        tau_proto::ToolType::Function => state.check_function_arguments_delta(output_index, delta),
        tau_proto::ToolType::Custom => state.check_custom_tool_input_delta(output_index, delta),
    }
}

fn check_tool_snapshot(
    state: &mut StreamState,
    output_index: usize,
    tool_type: tau_proto::ToolType,
    input: &str,
) -> Result<(), LlmError> {
    match tool_type {
        tau_proto::ToolType::Function => {
            state.check_function_arguments_snapshot(output_index, input)
        }
        tau_proto::ToolType::Custom => state.check_custom_tool_input_snapshot(output_index, input),
    }
}

fn apply_output_item_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    raw_item_json: Option<&str>,
    kind: OutputItemEventKind,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<(), LlmError> {
    let Some(item) = event.get("item") else {
        return Ok(());
    };
    let output_index = event_output_index(event);
    let mut changed = apply_output_item_tool(state, item, output_index, kind)?;
    changed |= apply_output_item_message(state, item, raw_item_json, output_index, kind)?;
    changed |= apply_output_item_reasoning(state, item, raw_item_json, output_index, kind);
    changed |= apply_output_item_compaction(state, item, raw_item_json, output_index, kind);
    changed |= apply_output_item_unknown(state, item, raw_item_json, output_index, kind);
    if changed {
        on_update(state);
    }
    Ok(())
}

fn apply_output_item_tool(
    state: &mut StreamState,
    item: &serde_json::Value,
    output_index: usize,
    kind: OutputItemEventKind,
) -> Result<bool, LlmError> {
    let Some(tool_type) = tool_type_from_output_item(item) else {
        return Ok(false);
    };
    if kind.is_done()
        && let Some(final_input) = final_tool_input(item, tool_type)
    {
        check_tool_snapshot(state, output_index, tool_type, final_input)?;
    }
    let call = state.tool_call_at_mut(output_index, tool_type);
    let mut changed = false;
    if let Some(id) = item["call_id"].as_str() {
        call.id = id.to_owned();
        changed = true;
    }
    if let Some(name) = item["name"].as_str() {
        call.name = name.to_owned();
        changed = true;
    }
    if call.arguments_json.is_empty()
        && let Some(final_input) = final_tool_input(item, tool_type)
    {
        call.arguments_json = final_input.to_owned();
        changed = true;
    }
    let responses_envelope = responses_tool_call_envelope(item);
    if call.responses_envelope != responses_envelope {
        call.responses_envelope = responses_envelope;
        changed = true;
    }
    Ok(changed)
}

fn responses_tool_call_envelope(item: &serde_json::Value) -> ResponsesToolCallEnvelope {
    let extra_fields = item.as_object().and_then(|object| {
        let extra: serde_json::Map<String, serde_json::Value> = object
            .iter()
            .filter(|(key, _)| !is_structured_responses_tool_call_field(key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        (!extra.is_empty()).then(|| json_to_cbor(&serde_json::Value::Object(extra)))
    });
    ResponsesToolCallEnvelope {
        item_id: item["id"].as_str().map(str::to_owned),
        status: item["status"].as_str().map(str::to_owned),
        extra_fields,
    }
}

fn is_structured_responses_tool_call_field(key: &str) -> bool {
    matches!(
        key,
        "type" | "id" | "status" | "call_id" | "name" | "arguments" | "input"
    )
}

fn tool_type_from_output_item(item: &serde_json::Value) -> Option<tau_proto::ToolType> {
    match item["type"].as_str() {
        Some("function_call") => Some(tau_proto::ToolType::Function),
        Some("custom_tool_call") => Some(tau_proto::ToolType::Custom),
        _ => None,
    }
}

fn final_tool_input(item: &serde_json::Value, tool_type: tau_proto::ToolType) -> Option<&str> {
    match tool_type {
        tau_proto::ToolType::Function => item["arguments"].as_str(),
        tau_proto::ToolType::Custom => item["input"].as_str(),
    }
}

fn apply_output_item_message(
    state: &mut StreamState,
    item: &serde_json::Value,
    raw_item_json: Option<&str>,
    output_index: usize,
    kind: OutputItemEventKind,
) -> Result<bool, LlmError> {
    if item["type"].as_str() != Some("message") {
        return Ok(false);
    }
    if !is_responses_assistant_message(item) {
        return Ok(false);
    }
    state.set_message_phase_at(output_index, parse_phase_from_item(item));
    if kind.is_done()
        && let Some(text) = message_text_from_output_item(item)
    {
        state.check_message_snapshot(output_index, &text)?;
        state.set_message_text_at(output_index, &text);
    }
    if kind.is_done()
        && let Some(raw_item_json) = raw_item_json
    {
        state.set_message_responses_raw_json_at(output_index, raw_item_json);
    }
    Ok(true)
}

fn apply_output_item_reasoning(
    state: &mut StreamState,
    item: &serde_json::Value,
    raw_item_json: Option<&str>,
    output_index: usize,
    kind: OutputItemEventKind,
) -> bool {
    // Capture reasoning items only on `output_item.done`, not on `added` — the
    // `added` event arrives before any summary parts/encrypted content stream in,
    // so its payload is just a stub. `done` carries the full item (id +
    // encrypted_content + summary) the harness needs to replay verbatim on the
    // next turn.
    //
    // The whole item is stashed as opaque JSON so a future wire-format change
    // (extra fields, schema rev) round-trips without code changes — same
    // Pi-style blob the harness re-emits on full-transcript replay.
    //
    // An item without `encrypted_content` is unreplayable: the server stores
    // reasoning only for `store: true` requests, and Codex forces `store: false`,
    // so a bare `rs_…` id in a later turn's `input[]` triggers `Item with id
    // 'rs_…' not found` and an 8-attempt retry loop. Skip those — losing
    // reasoning continuity on this turn is better than poisoning the chain.
    if kind.is_done()
        && item["type"].as_str() == Some("reasoning")
        && item["encrypted_content"].is_string()
    {
        let item_json = raw_or_canonical_item_json(raw_item_json, item);
        state.set_reasoning_item_json_at(output_index, &item_json);
        return true;
    }
    false
}

fn apply_output_item_compaction(
    state: &mut StreamState,
    item: &serde_json::Value,
    raw_item_json: Option<&str>,
    output_index: usize,
    kind: OutputItemEventKind,
) -> bool {
    if item["type"].as_str() != Some("compaction") {
        return false;
    }
    match kind {
        OutputItemEventKind::Added => state.start_compaction_item_at(output_index),
        OutputItemEventKind::Done => {
            let item_json = raw_or_canonical_item_json(raw_item_json, item);
            state.set_compaction_item_json_at(output_index, &item_json)
        }
    }
    true
}

fn apply_output_item_unknown(
    state: &mut StreamState,
    item: &serde_json::Value,
    raw_item_json: Option<&str>,
    output_index: usize,
    kind: OutputItemEventKind,
) -> bool {
    let Some(item_type) = item["type"].as_str() else {
        return false;
    };
    if is_known_output_item_type(item_type) {
        return false;
    }
    match kind {
        OutputItemEventKind::Added => state.reserve_output_item_at(output_index),
        OutputItemEventKind::Done => {
            let item_json = raw_or_canonical_item_json(raw_item_json, item);
            state.set_unknown_provider_item_json_at(output_index, &item_json);
        }
    }
    true
}

fn raw_or_canonical_item_json<'a>(
    raw_item_json: Option<&'a str>,
    item: &serde_json::Value,
) -> Cow<'a, str> {
    raw_item_json
        .map(Cow::Borrowed)
        .unwrap_or_else(|| Cow::Owned(item.to_string()))
}

fn is_known_output_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "function_call" | "custom_tool_call" | "message" | "reasoning" | "compaction"
    )
}

fn apply_terminal_event(state: &mut StreamState, event: &serde_json::Value) {
    state.provider_terminal_event = Some(event.clone());
    if state.input_tokens.is_none() {
        state.input_tokens = terminal_usage_u64(event, "input_tokens");
    }
    if state.cached_tokens.is_none() {
        state.cached_tokens = event
            .get("response")
            .and_then(|response| {
                response["usage"]["input_tokens_details"]["cached_tokens"].as_u64()
            })
            .or_else(|| event["usage"]["input_tokens_details"]["cached_tokens"].as_u64());
    }
    if state.output_tokens.is_none() {
        state.output_tokens = terminal_usage_u64(event, "output_tokens");
    }
    if state.response_id.is_none() {
        state.response_id = event
            .get("response")
            .and_then(|response| response["id"].as_str())
            .or_else(|| event["id"].as_str())
            .map(str::to_owned);
    }
}

fn terminal_usage_u64(event: &serde_json::Value, name: &str) -> Option<u64> {
    event
        .get("response")
        .and_then(|response| response["usage"][name].as_u64())
        .or_else(|| event["usage"][name].as_u64())
}

fn response_incomplete_error(event: &serde_json::Value) -> LlmError {
    let reason = event
        .get("response")
        .and_then(|r| r["incomplete_details"]["reason"].as_str())
        .unwrap_or("unknown reason");
    LlmError::HttpStatus(0, format!("response incomplete: {reason}"))
}

fn response_failed_error(event: &serde_json::Value) -> LlmError {
    let detail = event
        .get("response")
        .and_then(|r| {
            r["error"]["message"]
                .as_str()
                .or_else(|| r["error"]["code"].as_str())
        })
        .unwrap_or("unknown error");
    LlmError::HttpStatus(0, format!("response failed: {detail}"))
}

fn stream_error_event(event: &serde_json::Value) -> LlmError {
    let detail = event["error"]["message"]
        .as_str()
        .or_else(|| event["message"].as_str())
        .unwrap_or("unknown error");
    // Preserve the error code alongside the message so the retry classifier can
    // distinguish a transient transport hiccup from an account-level cap (usage
    // limit, rate limit, quota) — the latter must not be retried.
    //
    // The OpenAI Responses streaming `error` event uses `code` at the top level
    // (e.g. `code: "rate_limit_exceeded"`); some Codex variants nest an
    // `error.code` or older-style `error.type`. We check all three so an
    // upstream wording drift on one path doesn't silently re-enable the futile
    // retry loop on an account cap. The `(type=...)` suffix is a stable substring
    // contract matched by `LlmError::retry_after` and
    // `pool::is_recoverable_ws_error`.
    let error_code = event["error"]["code"]
        .as_str()
        .or_else(|| event["code"].as_str())
        .or_else(|| event["error"]["type"].as_str());
    let body = match error_code {
        Some(code) => format!("stream error: {detail} (type={code})"),
        None => format!("stream error: {detail}"),
    };
    LlmError::HttpStatus(0, body)
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<ResponsesInputItem>,
    /// `Some(true)` for HTTP+SSE transport — the only mode where the
    /// `stream` flag actually toggles framing. `None` on the WS
    /// transport, where the WS guide explicitly notes "transport-
    /// specific fields like `stream` and `background` are not used".
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    /// Always `Some(false)` on the ChatGPT Codex Responses endpoint —
    /// it rejects `store: true` even when chaining (see `build_request`).
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningRequest>,
    /// GPT-5 `text.verbosity` knob. Only set when the provider
    /// advertises `supports_verbosity`; otherwise omitted so older
    /// endpoints don't trip on an unknown field.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<TextRequest>,
    /// Optional opt-ins for richer response payloads. Currently only
    /// used to flip on `"reasoning.encrypted_content"`, which makes
    /// the model return an opaque per-`reasoning`-item blob the
    /// harness persists and replays on later turns. Omitted entirely
    /// when nothing's asked for so older endpoints don't trip on an
    /// unknown field.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    /// Optional upstream service tier (`fast` for Fast mode, `flex` for
    /// lower-priority service).
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<&'static str>,
    /// Optional server-side context management controls.
    #[serde(skip_serializing_if = "Option::is_none")]
    context_management: Option<Vec<ContextManagementRequest>>,
    /// Stateful-chain mode: points to the prior turn's `response.id`.
    /// When set, the upstream API carries reasoning context across
    /// turns and the request body only needs the *new* input
    /// (`messages[previous_response.message_index..]`). The win is a
    /// smaller request and faster TTFT — the server keeps the prior
    /// reasoning hot rather than re-deriving it from a replayed
    /// transcript. On Codex this works alongside `store: false`; on
    /// the public Responses API it requires `store: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ResponsesInputItem {
    Raw(Box<RawValue>),
    Json(serde_json::Value),
}

impl ResponsesInputItem {
    fn json(value: serde_json::Value) -> Self {
        Self::Json(value)
    }

    fn raw_json(raw_json: &str) -> Option<Self> {
        RawValue::from_string(raw_json.to_owned())
            .ok()
            .map(Self::Raw)
    }
}

#[derive(Serialize)]
struct ReasoningRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<ReasoningContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<&'static str>,
}

#[derive(Serialize)]
enum ReasoningContext {
    /// Server removes thinking after every user message.
    #[allow(dead_code)]
    #[serde(rename = "current_turn")]
    CurrentTurn,
    /// Server never removes thinking. Gives more cache hits.
    #[serde(rename = "all_turns")]
    AllTurns,
}

#[derive(Serialize)]
struct TextRequest {
    /// `low`/`medium`/`high` — see
    /// <https://developers.openai.com/api/docs/guides/deployment-checklist#set-up-textverbosity>.
    verbosity: &'static str,
}

#[derive(Serialize)]
struct ContextManagementRequest {
    #[serde(rename = "type")]
    ty: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    compact_threshold: Option<u64>,
}

fn build_request(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    cached_response_id: Option<&str>,
) -> ResponsesRequest {
    let instructions = if request.system_prompt.is_empty() {
        None
    } else {
        Some(request.system_prompt.to_owned())
    };

    // Stateful chaining: when a previous response is available, slice the
    // messages to just what's new since that response. The OpenAI
    // Responses API picks up the prior conversation from the stored
    // response — replaying its prefix would duplicate it. A defensive
    // cap to `messages.len()` covers the impossible-by-invariant case
    // of a stale index.
    let context_items: Vec<_> = request.context.flatten_iter().collect();
    let previous_response = cached_response_id.and_then(|id| {
        let mut next_item_index = context_items.len();
        for block in request.context.blocks.iter().rev() {
            match block {
                tau_proto::ContextBlock::AssistantResponse(response) => {
                    if response.provider_response_id.as_deref() == Some(id) {
                        // workaround for server side bug:
                        // server incorrectly build history if previous response id from
                        // compaction
                        //
                        // we solve by sending entire request in this case.
                        //
                        // this is pretty cheap, only happens for request after that normal
                        // previous_response_id should continue
                        if response
                            .output_items
                            .iter()
                            .any(|x| matches!(x, ContextItem::Compaction(_)))
                        {
                            return None;
                        }
                        return Some((id, next_item_index));
                    }
                    next_item_index = next_item_index.saturating_sub(response.output_items.len());
                }
                tau_proto::ContextBlock::UserInput(block) => {
                    next_item_index = next_item_index.saturating_sub(block.items.len());
                }
                tau_proto::ContextBlock::ToolResults(block) => {
                    next_item_index = next_item_index.saturating_sub(block.items.len());
                }
            }
        }
        None
    });
    let (input_items, previous_response_id): (&[ContextItem], Option<String>) =
        match previous_response {
            Some((id, next_item_index)) if next_item_index <= context_items.len() => {
                (&context_items[next_item_index..], Some(id.to_owned()))
            }
            _ => (context_items.as_slice(), None),
        };

    let input = build_input_items(config, input_items);

    let tools: Vec<serde_json::Value> = request.tools.iter().map(convert_tool_definition).collect();

    let tool_choice = match (request.tool_choice, tools.is_empty()) {
        // Harness-forced no-tools-this-turn: explicit `none` works
        // whether or not tools are declared (and is the whole point
        // of this branch — tools stay declared so the cache prefix
        // matches, the model is just told not to call them).
        (tau_proto::ToolChoice::None, _) => Some("none".to_owned()),
        // Default: only mention `tool_choice` when there are actual
        // tools — emitting `"auto"` on an empty list bumps the
        // request body for no reason and some endpoints reject it.
        (tau_proto::ToolChoice::Auto, false) => Some("auto".to_owned()),
        (tau_proto::ToolChoice::Auto, true) => None,
    };

    let effort = if config.supports_reasoning_effort {
        effort_wire(request.params.effort)
    } else {
        None
    };
    let summary = if config.supports_reasoning_summary {
        request.params.thinking_summary.as_openai_wire()
    } else {
        None
    };
    let reasoning = if effort.is_some() || summary.is_some() {
        Some(ReasoningRequest {
            effort,
            context: Some(ReasoningContext::AllTurns),
            summary,
        })
    } else {
        None
    };
    let text = if config.supports_verbosity {
        Some(TextRequest {
            verbosity: crate::common::verbosity_wire(request.params.verbosity),
        })
    } else {
        None
    };
    let prompt_cache_key = config
        .supports_prompt_cache_key
        .then(|| request.prompt_cache_key(&config.base_url));
    let include: Vec<&'static str> = if config.supports_encrypted_reasoning {
        vec!["reasoning.encrypted_content"]
    } else {
        Vec::new()
    };
    let context_management =
        request.compaction.map(|compaction| {
            vec![ContextManagementRequest {
                ty: "compaction",
                compact_threshold: Some(compaction.compact_threshold.unwrap_or_else(|| {
                    provider_default_compaction_threshold(config.context_window)
                })),
            }]
        });

    ResponsesRequest {
        model: config.model_id.clone(),
        instructions,
        input,
        // HTTP path always streams; WS path overrides this back to
        // `None` via [`ws_envelope`] before serializing.
        stream: Some(true),
        // ChatGPT/Codex rejects `store: true`, even when chaining with a
        // `previous_response_id`, so the provider owns this endpoint quirk.
        store: Some(config.surface.store_value()),
        tools,
        tool_choice,
        reasoning,
        text,
        include,
        prompt_cache_key,
        service_tier: request
            .params
            .service_tier
            .map(tau_proto::ServiceTier::as_wire),
        context_management,
        previous_response_id,
    }
}

fn provider_default_compaction_threshold(context_window: u64) -> u64 {
    (context_window * 9 / 10).max(1000)
}

fn build_input_items(
    config: &ResponsesConfig,
    input_items: &[ContextItem],
) -> Vec<ResponsesInputItem> {
    let input_items = if config.supports_compaction {
        trim_before_latest_compaction(input_items)
    } else {
        input_items
    };
    let mut input = Vec::new();
    for item in input_items {
        convert_context_item(item, config.supports_phase, &mut input);
    }
    input
}

fn trim_before_latest_compaction(input_items: &[ContextItem]) -> &[ContextItem] {
    input_items
        .iter()
        .rposition(|item| matches!(item, ContextItem::Compaction(_)))
        .map_or(input_items, |index| &input_items[index..])
}

/// WebSocket-side wrapper around a Responses request. The OpenAI
/// WS guide requires every client message to carry `type:
/// "response.create"` at the top level, while HTTP+SSE has no
/// envelope. `#[serde(flatten)]` keeps the body shape identical
/// across the two transports so request-build tests don't need a
/// separate fixture for each.
#[derive(Serialize)]
pub struct WsResponseCreate {
    #[serde(rename = "type")]
    ty: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    generate: Option<bool>,
    #[serde(flatten)]
    body: ResponsesRequest,
}

/// Build the JSON envelope to send over a WebSocket text frame for
/// one turn. Reuses the regular Responses request builder for the body — the
/// only deltas vs. the HTTP body are (a) the top-level `type` tag and (b)
/// dropping `stream` (transport-implicit on WS, per the WS guide).
pub(super) fn build_ws_envelope(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    cached_response_id: Option<&str>,
    generate: Option<bool>,
) -> WsResponseCreate {
    let mut body = build_request(config, request, cached_response_id);
    body.stream = None;
    WsResponseCreate {
        ty: "response.create",
        generate,
        body,
    }
}

// ---------------------------------------------------------------------------
// Phase capture
// ---------------------------------------------------------------------------

/// Extracts the assistant-phase label off a Responses-API `output_item.*`
/// item, when the item is an assistant `message`
/// carrying a known `phase` wire string. Returns `None` for items
/// that aren't messages, messages without a `phase` field, or wire
/// strings we don't recognize (forward-compatible: an unknown future
/// value just won't be persisted, rather than panicking).
fn parse_phase_from_item(item: &serde_json::Value) -> Option<tau_proto::MessagePhase> {
    if item.get("type").and_then(serde_json::Value::as_str)? != "message" {
        return None;
    }
    match item.get("phase")?.as_str()? {
        "commentary" => Some(tau_proto::MessagePhase::Commentary),
        "final_answer" => Some(tau_proto::MessagePhase::FinalAnswer),
        _ => None,
    }
}

fn message_text_from_output_item(item: &serde_json::Value) -> Option<String> {
    let mut text = String::new();

    for part in item
        .get("content")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if is_responses_text_part(part)
            && let Some(part_text) = responses_text_part_text(part)
        {
            text.push_str(part_text);
        }
    }

    if text.is_empty() { None } else { Some(text) }
}

fn convert_tool_definition(tool: &tau_proto::ToolDefinition) -> serde_json::Value {
    let model_visible_name = tool.model_visible_name.as_ref().unwrap_or(&tool.name);
    match tool.tool_type {
        tau_proto::ToolType::Function => {
            let mut wire = serde_json::json!({
                "type": "function",
                "name": encode_tool_name(model_visible_name),
                "strict": serde_json::Value::Null,
            });
            if let Some(ref desc) = tool.description {
                wire["description"] = serde_json::Value::String(desc.clone());
            }
            if let Some(ref params) = tool.parameters {
                wire["parameters"] = params.clone();
            }
            wire
        }
        tau_proto::ToolType::Custom => {
            let mut wire = serde_json::json!({
                "type": "custom",
                "name": encode_tool_name(model_visible_name),
            });
            if let Some(ref desc) = tool.description {
                wire["description"] = serde_json::Value::String(desc.clone());
            }
            if let Some(ref format) = tool.format {
                wire["format"] = serialize_tool_format(format);
            }
            wire
        }
    }
}

fn serialize_tool_format(format: &tau_proto::ToolFormat) -> serde_json::Value {
    match format {
        tau_proto::ToolFormat::Text => serde_json::json!({
            "type": "text",
        }),
        tau_proto::ToolFormat::Grammar { syntax, definition } => serde_json::json!({
            "type": "grammar",
            "syntax": match syntax {
                tau_proto::ToolGrammarSyntax::Lark => "lark",
                tau_proto::ToolGrammarSyntax::Regex => "regex",
            },
            "definition": definition,
        }),
    }
}

// ---------------------------------------------------------------------------
// Tool name encoding
// ---------------------------------------------------------------------------

/// Encode tool name for the API: replace non-`[a-zA-Z0-9_-]` with `_`.
fn encode_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Conversation conversion
// ---------------------------------------------------------------------------

fn convert_context_item(
    item: &ContextItem,
    supports_phase: bool,
    out: &mut Vec<ResponsesInputItem>,
) {
    match item {
        ContextItem::Message(msg) if msg.role == ContextRole::User => {
            convert_user_message(msg, out);
        }
        ContextItem::Message(msg) if msg.role == ContextRole::Assistant => {
            convert_assistant_message(msg, supports_phase, out);
        }
        ContextItem::ToolCall(call) => convert_tool_call_item(call, out),
        ContextItem::ToolResult(result) => convert_tool_result_item(result, out),
        ContextItem::ReasoningText(_) => {}
        ContextItem::Reasoning(item) => {
            convert_opaque_provider_item(item, out);
        }
        ContextItem::CompactionTrigger => {
            out.push(ResponsesInputItem::json(serde_json::json!({
                "type": "compaction_trigger",
            })));
        }
        ContextItem::Compaction(item) | ContextItem::UnknownProviderItem(item) => {
            convert_opaque_provider_item(item, out);
        }
        ContextItem::Message(_) => {}
    }
}

fn convert_opaque_provider_item(
    item: &tau_proto::OpaqueProviderItem,
    out: &mut Vec<ResponsesInputItem>,
) {
    if let Some(raw_item) = item
        .raw_json
        .as_deref()
        .and_then(ResponsesInputItem::raw_json)
    {
        out.push(raw_item);
        return;
    }
    out.push(ResponsesInputItem::json(cbor_to_json(&item.value)));
}

fn convert_user_message(msg: &MessageItem, out: &mut Vec<ResponsesInputItem>) {
    // Collect text blocks into one user message, emit tool results separately.
    let text_items: Vec<serde_json::Value> = msg
        .content
        .iter()
        .map(|block| match block {
            ContentPart::Text { text } => serde_json::json!({
                "type": "input_text",
                "text": text,
            }),
        })
        .collect();
    if !text_items.is_empty() {
        out.push(ResponsesInputItem::json(serde_json::json!({
            "role": "user",
            "content": text_items,
        })));
    }
}

fn convert_assistant_message(
    msg: &MessageItem,
    supports_phase: bool,
    out: &mut Vec<ResponsesInputItem>,
) {
    let text_parts = assistant_message_text_parts(msg);
    if text_parts.is_empty() {
        return;
    }

    if let Some(raw_item) = convert_raw_assistant_message(msg, supports_phase, &text_parts) {
        out.push(raw_item);
        return;
    }

    let mut item = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "content": [rebuilt_output_text_part(&text_parts.join("\n"))],
    });
    if let Some(phase) = assistant_message_phase_wire(msg, supports_phase) {
        item["phase"] = serde_json::Value::String(phase.to_owned());
    }
    out.push(ResponsesInputItem::json(item));
}

fn assistant_message_text_parts(msg: &MessageItem) -> Vec<&str> {
    msg.content
        .iter()
        .map(|block| match block {
            ContentPart::Text { text } => text.as_str(),
        })
        .collect()
}

fn convert_raw_assistant_message(
    msg: &MessageItem,
    supports_phase: bool,
    text_parts: &[&str],
) -> Option<ResponsesInputItem> {
    let raw_json = msg.responses_raw_json.as_deref()?;
    if raw_assistant_message_matches_replay(msg, supports_phase, text_parts, raw_json) {
        return ResponsesInputItem::raw_json(raw_json);
    }

    let mut item: serde_json::Value = serde_json::from_str(raw_json).ok()?;
    update_raw_assistant_message(&mut item, msg, supports_phase, text_parts)?;
    Some(ResponsesInputItem::json(item))
}

fn raw_assistant_message_matches_replay(
    msg: &MessageItem,
    supports_phase: bool,
    text_parts: &[&str],
    raw_json: &str,
) -> bool {
    let Ok(item) = serde_json::from_str::<serde_json::Value>(raw_json) else {
        return false;
    };
    if !is_responses_assistant_message(&item) {
        return false;
    }
    raw_message_text_matches(&item, text_parts)
        && raw_message_phase_matches(&item, msg, supports_phase)
}

fn is_responses_assistant_message(item: &serde_json::Value) -> bool {
    item.get("type").and_then(serde_json::Value::as_str) == Some("message")
        && item.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
}

fn raw_message_phase_matches(
    item: &serde_json::Value,
    msg: &MessageItem,
    supports_phase: bool,
) -> bool {
    match assistant_message_phase_wire(msg, supports_phase) {
        Some(expected) => item.get("phase").and_then(serde_json::Value::as_str) == Some(expected),
        None => item.get("phase").is_none(),
    }
}

fn update_raw_assistant_message(
    item: &mut serde_json::Value,
    msg: &MessageItem,
    supports_phase: bool,
    text_parts: &[&str],
) -> Option<()> {
    if !is_responses_assistant_message(item) {
        return None;
    }

    let object = item.as_object_mut()?;
    update_raw_message_text_parts(object, text_parts);
    match assistant_message_phase_wire(msg, supports_phase) {
        Some(phase) => {
            object.insert(
                "phase".to_owned(),
                serde_json::Value::String(phase.to_owned()),
            );
        }
        None => {
            object.remove("phase");
        }
    }
    Some(())
}

fn raw_message_text_parts(item: &serde_json::Value) -> Option<Vec<&str>> {
    let content = item.get("content")?.as_array()?;
    let mut parts = Vec::new();
    for part in content {
        if is_responses_text_part(part) {
            parts.push(part.get("text")?.as_str()?);
        }
    }
    Some(parts)
}

fn raw_message_text_matches(item: &serde_json::Value, text_parts: &[&str]) -> bool {
    // Captured Responses items may split one Tau semantic assistant text across
    // multiple provider content parts. Treat exact part equality and identical
    // concatenated text as a match so unchanged replay preserves provider-owned
    // part ids, annotations, and unknown part fields.
    raw_message_text_parts(item).is_some_and(|raw_parts| {
        raw_parts == text_parts || raw_parts.concat() == text_parts.join("\n")
    })
}

fn update_raw_message_text_parts(
    object: &mut serde_json::Map<String, serde_json::Value>,
    text_parts: &[&str],
) {
    let Some(content) = object
        .get_mut("content")
        .and_then(serde_json::Value::as_array_mut)
    else {
        object.insert("content".to_owned(), rebuilt_message_content(text_parts));
        return;
    };

    let text_indexes = content
        .iter()
        .enumerate()
        .filter_map(|(index, part)| is_responses_text_part(part).then_some(index))
        .collect::<Vec<_>>();
    if text_indexes.len() == text_parts.len() {
        for (index, text) in text_indexes.into_iter().zip(text_parts.iter().copied()) {
            if let Some(part) = content[index].as_object_mut() {
                part.insert(
                    "text".to_owned(),
                    serde_json::Value::String(text.to_owned()),
                );
            }
        }
        return;
    }

    *content = text_parts
        .iter()
        .map(|text| rebuilt_output_text_part(text))
        .collect();
}

fn is_responses_text_part(part: &serde_json::Value) -> bool {
    matches!(
        part.get("type").and_then(serde_json::Value::as_str),
        Some("output_text") | Some("text")
    ) && responses_text_part_text(part).is_some()
}

fn responses_text_part_text(part: &serde_json::Value) -> Option<&str> {
    part.get("text").and_then(serde_json::Value::as_str)
}

fn rebuilt_message_content(text_parts: &[&str]) -> serde_json::Value {
    serde_json::Value::Array(
        text_parts
            .iter()
            .map(|text| rebuilt_output_text_part(text))
            .collect(),
    )
}

fn rebuilt_output_text_part(text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "output_text",
        "text": text,
        "annotations": [],
    })
}

fn assistant_message_phase_wire(msg: &MessageItem, supports_phase: bool) -> Option<&'static str> {
    // `phase` (when the backend supports it): stamp every assistant `message`
    // item we replay. The stored `msg.phase` is preferred; turns from before
    // this field existed (or from non-Codex paths) get the doc-recommended
    // `final_answer` default — the OpenAI deployment checklist explicitly
    // calls this out as the fallback for missing phase on history.
    supports_phase.then(|| {
        msg.phase
            .unwrap_or(tau_proto::MessagePhase::FinalAnswer)
            .as_openai_wire()
    })
}

fn convert_tool_call_item(call: &ToolCallItem, out: &mut Vec<ResponsesInputItem>) {
    match call.tool_type {
        tau_proto::ToolType::Function => {
            out.push(ResponsesInputItem::json(convert_function_call_item(call)));
        }
        tau_proto::ToolType::Custom => {
            out.push(ResponsesInputItem::json(convert_custom_tool_call_item(
                call,
            )));
        }
    }
}

fn convert_function_call_item(call: &ToolCallItem) -> serde_json::Value {
    let id_str = call.call_id.as_str();
    let arguments = function_call_arguments_json(call);
    let mut item = responses_tool_call_base(call, "fc_");
    item.insert("type".to_owned(), serde_json::json!("function_call"));
    item.insert("call_id".to_owned(), serde_json::json!(id_str));
    item.insert(
        "name".to_owned(),
        serde_json::json!(encode_tool_name(call.name.as_str())),
    );
    item.insert("arguments".to_owned(), serde_json::json!(arguments));
    serde_json::Value::Object(item)
}

fn function_call_arguments_json(call: &ToolCallItem) -> String {
    call.raw_arguments_json.clone().unwrap_or_else(|| {
        let args_json = cbor_to_json(&call.arguments);
        serde_json::to_string(&args_json).unwrap_or_default()
    })
}

fn convert_custom_tool_call_item(call: &ToolCallItem) -> serde_json::Value {
    let id_str = call.call_id.as_str();
    let input = match &call.arguments {
        tau_proto::CborValue::Text(text) => text.clone(),
        other => serde_json::to_string(&cbor_to_json(other)).unwrap_or_default(),
    };
    let mut item = responses_tool_call_base(call, "ctc_");
    item.insert("type".to_owned(), serde_json::json!("custom_tool_call"));
    item.insert("call_id".to_owned(), serde_json::json!(id_str));
    item.insert(
        "name".to_owned(),
        serde_json::json!(encode_tool_name(call.name.as_str())),
    );
    item.insert("input".to_owned(), serde_json::json!(input));
    serde_json::Value::Object(item)
}

fn responses_tool_call_base(
    call: &ToolCallItem,
    fallback_id_prefix: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut item = serde_json::Map::new();
    if let Some(envelope) = &call.responses_envelope {
        if let Some(extra_fields) = &envelope.extra_fields
            && let serde_json::Value::Object(extra) = cbor_to_json(extra_fields)
        {
            item.extend(extra);
        }
        if let Some(status) = &envelope.status {
            item.insert("status".to_owned(), serde_json::json!(status));
        }
    }
    let item_id = call
        .responses_envelope
        .as_ref()
        .and_then(|envelope| envelope.item_id.as_deref())
        .map(str::to_owned)
        .unwrap_or_else(|| prefixed_provider_item_id(call.call_id.as_str(), fallback_id_prefix));
    item.insert("id".to_owned(), serde_json::json!(item_id));
    item
}

fn prefixed_provider_item_id(id: &str, prefix: &str) -> String {
    if id.starts_with(prefix) {
        id.to_owned()
    } else {
        format!("{prefix}{id}")
    }
}

fn convert_tool_result_item(result: &ToolResultItem, out: &mut Vec<ResponsesInputItem>) {
    let output_type = match result.tool_type {
        tau_proto::ToolType::Function => "function_call_output",
        tau_proto::ToolType::Custom => "custom_tool_call_output",
    };
    out.push(ResponsesInputItem::json(serde_json::json!({
        "type": output_type,
        "call_id": result.call_id,
        "output": convert_tool_result_output(result),
    })));
}

fn convert_tool_result_output(result: &ToolResultItem) -> String {
    match &result.status {
        ToolResultStatus::Success => result.output.render(),
        ToolResultStatus::Error { message } => render_error_tool_result(&result.output, message),
        ToolResultStatus::Cancelled { reason } => render_cancelled_tool_result(reason),
    }
}

fn render_error_tool_result(output: &tau_proto::ToolResponse, message: &str) -> String {
    let mut response = output.clone();
    response.headers.insert(
        0,
        ToolResponseHeader {
            key: "error".to_owned(),
            value: message.to_owned(),
        },
    );
    response.render()
}

fn render_cancelled_tool_result(reason: &str) -> String {
    tau_proto::ToolResponse {
        raw: tau_proto::CborValue::Null,
        headers: vec![ToolResponseHeader {
            key: "cancelled".to_owned(),
            value: reason.to_owned(),
        }],
        body: String::new(),
    }
    .render()
}

fn duration_micros_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

pub(super) fn scale_delay(delay: Duration, speed: f64) -> Duration {
    let speed = if speed.is_finite() && 0.0 < speed {
        speed
    } else {
        1.0
    };
    Duration::from_secs_f64(delay.as_secs_f64() / speed)
}

#[cfg(test)]
mod tests;
