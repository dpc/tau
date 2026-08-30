//! Private bounded diagnostics for generic public Responses attempts.

use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::{cell::RefCell, thread_local};

use serde::Serialize;
use serde_json::Value;
use tau_provider::debug_capture_writer as path_tau_provider_debug_capture_writer;

use super::{
    AttemptConfig, AttemptModel, AttemptProgress, Error, ProviderTokenUsage, RequestBody, State,
    Transport,
};

/// Maximum raw provider-event JSON retained for one explicitly enabled response
/// capture.
pub(super) const MAX_RESPONSE_EVENT_BYTES: usize = 512 * 1024;
/// Maximum number of raw provider events retained for one response capture.
const MAX_RESPONSE_EVENTS: usize = 4_096;
/// Maximum uncompressed JSON submitted for one explicitly enabled debug
/// capture.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
/// Safe replacement for any string containing an embedded image data URL.
const IMAGE_OMISSION: &str = "[image data omitted]";
/// Safe replacement for the exact credential dispatched by the transport.
const CREDENTIAL_OMISSION: &str = "[REDACTED]";

type Capture = path_tau_provider_debug_capture_writer::ProviderDebugCapture;
type CaptureClass = path_tau_provider_debug_capture_writer::ProviderDebugCaptureClass;
type CaptureSink = Arc<dyn Fn(Capture) + Send + Sync>;

#[cfg(test)]
thread_local! {
    /// Per-test override used to exercise the public attempt entry point.
    static TEST_CAPTURE_SINK: RefCell<Option<CaptureSink>> = const { RefCell::new(None) };
}

/// Private diagnostic state and bounded best-effort capture submission for one
/// attempt.
#[derive(Clone)]
pub(super) struct DebugCapture {
    /// Whether the extension permitted private capture for this durable prompt.
    enabled: bool,
    /// Shared bounded response events retained by the transport parser.
    events: Arc<Mutex<Events>>,
    /// Production writer or an injected deterministic test sink.
    sink: CaptureSink,
}

impl fmt::Debug for DebugCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DebugCapture")
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl Default for DebugCapture {
    fn default() -> Self {
        Self::new(false)
    }
}

/// Bounded raw event state shared across the attempt and its outcome handler.
#[derive(Debug, Default)]
struct Events {
    /// Raw provider events retained within both count and byte limits.
    values: Vec<Value>,
    /// Sum of original raw JSON bytes retained in `values`.
    bytes: usize,
    /// Whether one or more events did not fit the count or byte limit.
    truncated: bool,
}

impl DebugCapture {
    /// Construct capture state for one attempt under the extension's policy.
    pub(super) fn new(enabled: bool) -> Self {
        #[cfg(test)]
        let sink = TEST_CAPTURE_SINK
            .with(|sink| sink.borrow().clone())
            .unwrap_or_else(|| Arc::new(submit_capture));
        #[cfg(not(test))]
        let sink = Arc::new(submit_capture);
        Self::with_sink(enabled, sink)
    }

    /// Construct capture state with an injected deterministic sink.
    #[cfg(test)]
    pub(super) fn with_test_sink(enabled: bool, sink: CaptureSink) -> Self {
        Self::with_sink(enabled, sink)
    }

    /// Run the public attempt entry point with a thread-local deterministic
    /// sink.
    #[cfg(test)]
    pub(super) fn with_test_sink_scope<T>(sink: CaptureSink, run: impl FnOnce() -> T) -> T {
        struct ResetSink(Option<CaptureSink>);

        impl Drop for ResetSink {
            fn drop(&mut self) {
                TEST_CAPTURE_SINK.with(|sink| {
                    *sink.borrow_mut() = self.0.take();
                });
            }
        }

        let previous = TEST_CAPTURE_SINK.with(|current| current.borrow_mut().replace(sink));
        let _reset = ResetSink(previous);
        run()
    }

    /// Construct capture state with the selected submission sink.
    fn with_sink(enabled: bool, sink: CaptureSink) -> Self {
        Self {
            enabled,
            events: Arc::default(),
            sink,
        }
    }

    /// Retain one parsed provider event only while private diagnostics are
    /// enabled and the event fits both response bounds.
    pub(super) fn record_event(&self, event: &Value, raw_json: &str) {
        if !self.enabled {
            return;
        }
        let Ok(mut events) = self.events.lock() else {
            return;
        };
        let next_bytes = events.bytes.saturating_add(raw_json.len());
        if events.values.len() < MAX_RESPONSE_EVENTS && next_bytes <= MAX_RESPONSE_EVENT_BYTES {
            events.values.push(event.clone());
            events.bytes = next_bytes;
        } else {
            events.truncated = true;
        }
    }

    /// Submit the finalized HTTP/SSE request at its send boundary.
    pub(super) fn submit_request(
        &self,
        prompt: &tau_proto::AgentPromptCreated,
        config: &AttemptConfig,
        model: &AttemptModel,
        body: &RequestBody,
    ) {
        if !self.enabled {
            return;
        }
        self.submit(
            prompt,
            config,
            Self::request_class(config.transport),
            &RequestCapture {
                common: CommonCapture::new(prompt, config, model),
                body,
            },
        );
    }

    /// Submit the exact WebSocket `response.create` payload at frame-send.
    pub(super) fn submit_wire_request(
        &self,
        prompt: &tau_proto::AgentPromptCreated,
        config: &AttemptConfig,
        model: &AttemptModel,
        body: &impl Serialize,
    ) {
        if !self.enabled {
            return;
        }
        self.submit(
            prompt,
            config,
            Self::request_class(config.transport),
            &RequestCapture {
                common: CommonCapture::new(prompt, config, model),
                body,
            },
        );
    }

    /// Submit the successful response after terminal validation.
    pub(super) fn submit_response(
        &self,
        prompt: &tau_proto::AgentPromptCreated,
        config: &AttemptConfig,
        model: &AttemptModel,
        state: &State,
        stop_reason: tau_proto::ProviderStopReason,
    ) {
        if !self.enabled {
            return;
        }
        let Ok(events) = self.events.lock() else {
            return;
        };
        self.submit(
            prompt,
            config,
            Self::response_class(config.transport),
            &ResponseCapture {
                common: CommonCapture::new(prompt, config, model),
                provider_response_id: state.response_id.as_deref(),
                usage: &state.usage,
                stop_reason,
                response_bytes_received: state.bytes,
                raw_events: &events.values,
                raw_events_truncated: events.truncated,
            },
        );
    }

    /// Submit bounded metadata for an unsuccessful non-cancellation attempt.
    pub(super) fn submit_error(
        &self,
        prompt: &tau_proto::AgentPromptCreated,
        config: &AttemptConfig,
        model: &AttemptModel,
        error: &Error,
        progress: &AttemptProgress,
    ) {
        if !self.enabled || matches!(error, Error::Canceled) {
            return;
        }
        let metadata = error_metadata(error);
        self.submit(
            prompt,
            config,
            Self::response_class(config.transport),
            &ErrorCapture {
                common: CommonCapture::new(prompt, config, model),
                response_bytes_received: progress.response_bytes_received,
                error: &metadata,
            },
        );
    }

    /// Serialize, sanitize, and submit one sensitive artifact without ever
    /// retaining serialized JSON beyond the configured ceiling.
    fn submit(
        &self,
        prompt: &tau_proto::AgentPromptCreated,
        config: &AttemptConfig,
        class: CaptureClass,
        metadata: &impl Serialize,
    ) {
        if !self.enabled {
            return;
        }
        let serialized = match serialize_capped(metadata, MAX_CAPTURE_BYTES) {
            Ok(serialized) => {
                let Ok(mut value) = serde_json::from_slice::<Value>(&serialized) else {
                    return;
                };
                sanitize_capture_value(&mut value, &config.api_key);
                match serialize_capped(&value, MAX_CAPTURE_BYTES) {
                    Ok(sanitized) => sanitized,
                    Err(CapExceeded) => truncation_record(),
                }
            }
            Err(CapExceeded) => truncation_record(),
        };
        (self.sink)(Capture::new(
            prompt.session_id.clone(),
            prompt.agent_prompt_id.clone(),
            class,
            serialized,
        ));
    }

    /// Return the capture class for the request side of a configured transport.
    pub(super) fn request_class(transport: Transport) -> CaptureClass {
        match transport {
            Transport::Sse => CaptureClass::HttpSseRequest,
            Transport::Websocket => CaptureClass::WebsocketRequest,
        }
    }

    /// Return the capture class for the response or error side of a configured
    /// transport.
    pub(super) fn response_class(transport: Transport) -> CaptureClass {
        match transport {
            Transport::Sse => CaptureClass::HttpSseResponse,
            Transport::Websocket => CaptureClass::WebsocketResponse,
        }
    }
}

/// Fields common to every public Responses capture.
#[derive(Serialize)]
struct CommonCapture<'a> {
    /// Durable session identifier used by the shared writer.
    session_id: &'a str,
    /// Prompt identifier used by the shared writer.
    agent_prompt_id: &'a str,
    /// Stable transport label.
    transport: &'static str,
    /// Stable backend label.
    backend: &'static str,
    /// Configured model identifier.
    model: &'a str,
    /// Flattened prompt item count.
    context_item_count: usize,
    /// Declared tool count.
    tool_count: usize,
    /// Selected tool policy.
    tool_choice: tau_proto::ToolChoice,
}

impl<'a> CommonCapture<'a> {
    /// Borrow common metadata only after capture has been enabled.
    fn new(
        prompt: &'a tau_proto::AgentPromptCreated,
        config: &AttemptConfig,
        model: &'a AttemptModel,
    ) -> Self {
        Self {
            session_id: &prompt.session_id,
            agent_prompt_id: &prompt.agent_prompt_id,
            transport: transport_label(config.transport),
            backend: "responses",
            model: &model.id,
            context_item_count: prompt.context.flatten_iter().count(),
            tool_count: prompt.tools.len(),
            tool_choice: prompt.tool_choice,
        }
    }
}

/// Borrowed request capture, avoiding an unbounded body clone.
#[derive(Serialize)]
struct RequestCapture<'a, T> {
    /// Shared attempt metadata.
    #[serde(flatten)]
    common: CommonCapture<'a>,
    /// Final body dispatched by the selected transport.
    body: &'a T,
}

/// Borrowed successful-response capture.
#[derive(Serialize)]
struct ResponseCapture<'a> {
    /// Shared attempt metadata.
    #[serde(flatten)]
    common: CommonCapture<'a>,
    /// Optional provider response identifier.
    provider_response_id: Option<&'a str>,
    /// Parsed terminal usage.
    usage: &'a Option<ProviderTokenUsage>,
    /// Validated provider stop reason.
    stop_reason: tau_proto::ProviderStopReason,
    /// Total transport response bytes.
    response_bytes_received: u64,
    /// Bounded raw provider events.
    raw_events: &'a [Value],
    /// Whether raw events exceeded their separate bound.
    raw_events_truncated: bool,
}

/// Borrowed unsuccessful-response capture.
#[derive(Serialize)]
struct ErrorCapture<'a> {
    /// Shared attempt metadata.
    #[serde(flatten)]
    common: CommonCapture<'a>,
    /// Response bytes received before failure.
    response_bytes_received: u64,
    /// Safe typed failure metadata.
    error: &'a Value,
}

/// Writer which retains at most `limit` bytes and fails on byte `limit + 1`.
struct CappedWriter {
    /// Retained output bytes.
    bytes: Vec<u8>,
    /// Inclusive maximum retained size.
    limit: usize,
}

impl Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.limit.saturating_sub(self.bytes.len()) < bytes.len() {
            return Err(io::Error::other(CapExceeded));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Marker returned when serialization attempts to emit byte `cap + 1`.
#[derive(Clone, Copy, Debug)]
struct CapExceeded;

impl fmt::Display for CapExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("capture JSON exceeded its byte limit")
    }
}

impl std::error::Error for CapExceeded {}

/// Serialize directly into a bounded writer.
fn serialize_capped(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, CapExceeded> {
    let mut writer = CappedWriter {
        bytes: Vec::with_capacity(limit.min(64 * 1024)),
        limit,
    };
    serde_json::to_writer_pretty(&mut writer, value).map_err(|_| CapExceeded)?;
    Ok(writer.bytes)
}

/// Return fixed content-free metadata for an oversized capture.
fn truncation_record() -> Vec<u8> {
    br#"{"capture_truncated":true}"#.to_vec()
}

/// Submit one production capture through the shared best-effort writer.
fn submit_capture(capture: Capture) {
    path_tau_provider_debug_capture_writer::submit_provider_debug_capture(capture);
}

/// Remove image data URLs and redact exact configured credentials from private
/// capture JSON. Any projected-key collision replaces the complete object with
/// content-free omission metadata.
fn sanitize_capture_value(value: &mut Value, secret: &str) {
    match value {
        Value::String(text) => *text = sanitize_text(text, secret),
        Value::Array(values) => {
            for value in values {
                sanitize_capture_value(value, secret);
            }
        }
        Value::Object(values) => {
            let mut sanitized = serde_json::Map::new();
            for (key, mut value) in std::mem::take(values) {
                sanitize_capture_value(&mut value, secret);
                let key = sanitize_text(&key, secret);
                if sanitized.insert(key, value).is_some() {
                    *values = serde_json::Map::from_iter([(
                        "capture_omitted".to_owned(),
                        Value::String("projected_key_collision".to_owned()),
                    )]);
                    return;
                }
            }
            *values = sanitized;
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Sanitize one JSON string or key without partial data-URL disclosure.
fn sanitize_text(text: &str, secret: &str) -> String {
    if contains_ascii_case_insensitive(text, b"data:image/") {
        IMAGE_OMISSION.to_owned()
    } else if secret.is_empty() {
        text.to_owned()
    } else {
        text.replace(secret, CREDENTIAL_OMISSION)
    }
}

/// Return whether `text` contains one ASCII token under case-insensitive URI
/// scheme and media-type comparison.
fn contains_ascii_case_insensitive(text: &str, needle: &[u8]) -> bool {
    text.as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Return the stable diagnostic label for a configured transport.
fn transport_label(transport: Transport) -> &'static str {
    match transport {
        Transport::Sse => "http-sse",
        Transport::Websocket => "websocket",
    }
}

/// Build bounded private metadata for an unsuccessful attempt without logging
/// it.
fn error_metadata(error: &Error) -> Value {
    match error {
        Error::Http(status, body) => serde_json::json!({
            "kind": "http", "http_status": status, "body": body,
        }),
        Error::Provider { status, code } => serde_json::json!({
            "kind": "provider", "http_status": status, "code": code,
        }),
        Error::Outbound(error) => serde_json::json!({
            "kind": "outbound",
            "route": format!("{:?}", error.route()),
            "phase": format!("{:?}", error.phase()),
            "category": format!("{:?}", error.kind()),
        }),
        Error::EmptyResponse => serde_json::json!({"kind": "empty_response"}),
        Error::Canceled => serde_json::json!({"kind": "canceled"}),
        Error::InvalidRequest => serde_json::json!({"kind": "invalid_request"}),
        Error::UnsupportedTool => serde_json::json!({"kind": "unsupported_tool"}),
        Error::UnsupportedOutput => serde_json::json!({"kind": "unsupported_output"}),
        Error::RepetitionDetected(_) => serde_json::json!({"kind": "repetition_detected"}),
        Error::Json => serde_json::json!({"kind": "json"}),
        Error::StreamFailure => serde_json::json!({"kind": "stream_failure"}),
    }
}

#[cfg(test)]
mod tests;
