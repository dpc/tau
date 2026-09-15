//! Private, bounded evidence of received text, independent of parser
//! acceptance.

use serde_json::json;
use tau_provider::debug_capture_writer::{ProviderDebugCapture, ProviderDebugCaptureClass};

use crate::attempt_failure::DispatchCorrelation;
use crate::common::{LlmError, PromptPayload, StreamState};

/// Maximum retained prefix bytes before JSON escaping.
const PREFIX_BYTES: usize = 1024 * 1024;
/// Bound allocation overhead even for tiny provider messages.
const PREFIX_EVENTS: usize = 4096;
/// Matches the live transport's complete text-message ceiling.
const LAST_EVENT_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 prefix retained from internal error-chain formatting.
const ERROR_DETAIL_BYTES: usize = 16 * 1024;

/// Exact received prefix plus the latest event when that prefix is truncated.
///
/// This is not VCR recording: overflow never returns a provider error. The
/// response owner submits it after every normal return, before propagating
/// errors.
pub(super) struct ResponseCapture {
    /// Whole original text messages, never parsed and reserialized.
    raw_events: Vec<String>,
    /// Bytes retained in the prefix, excluding allocation overhead.
    prefix_bytes: usize,
    /// Number of text messages observed before returning.
    received_events: u64,
    /// Total text bytes observed, not just retained bytes.
    received_bytes: u64,
    /// Sticky indication that the prefix stopped retaining complete messages.
    truncated: bool,
    /// Latest original message, preserving the rejection site after prefix
    /// overflow.
    last_received_event: Option<String>,
    /// Whether even the latest event exceeded the capture ceiling.
    last_event_truncated: bool,
    /// Decode failure detail which would otherwise be lost in transport
    /// mapping.
    decode_error: Option<String>,
}

impl ResponseCapture {
    /// Select only existing exact-capture policy; disabled attempts allocate
    /// nothing.
    pub(super) fn selected(enabled: bool) -> Option<Self> {
        enabled.then(|| Self {
            raw_events: Vec::new(),
            prefix_bytes: 0,
            received_events: 0,
            received_bytes: 0,
            truncated: false,
            last_received_event: None,
            last_event_truncated: false,
            decode_error: None,
        })
    }

    /// Observe original text before decoding, shape validation, or resource
    /// admission.
    pub(super) fn record(&mut self, text: &str) {
        self.received_events = self.received_events.saturating_add(1);
        self.received_bytes = self.received_bytes.saturating_add(text.len() as u64);
        if !self.truncated
            && self.raw_events.len() < PREFIX_EVENTS
            && text.len() <= PREFIX_BYTES.saturating_sub(self.prefix_bytes)
        {
            self.prefix_bytes += text.len();
            self.raw_events.push(text.to_owned());
            return;
        }
        self.truncated = true;
        let end = text.floor_char_boundary(LAST_EVENT_BYTES.min(text.len()));
        self.last_received_event = Some(text[..end].to_owned());
        self.last_event_truncated = end != text.len();
    }

    /// Preserve the original JSON decoder error before sanitized transport
    /// mapping.
    pub(super) fn decode_failed(&mut self, error: &serde_json::Error) {
        self.decode_error = Some(error.to_string());
    }

    /// Submit received evidence through the same private response FIFO as
    /// terminal captures, without changing the result or draining any
    /// unread provider data.
    pub(super) fn submit(
        self,
        agent_prompt_id: &str,
        request: &PromptPayload<'_>,
        correlation: Option<&DispatchCorrelation>,
        response_mode: super::ws::ResponseMode,
        result: &Result<StreamState, LlmError>,
        submit: impl FnOnce(ProviderDebugCapture),
    ) {
        let Ok(prompt_id) = tau_proto::AgentPromptId::parse(agent_prompt_id) else {
            tracing::warn!(target: crate::LOG_TARGET, "invalid response capture prompt id; dropping capture");
            return;
        };
        let mut metadata = json!({
            "record_kind": "received_response",
            "session_id": request.session_id,
            "agent_prompt_id": agent_prompt_id,
            "backend": "responses",
            "transport": "websocket",
            "response_mode": match response_mode {
                super::ws::ResponseMode::Ordinary => "ordinary",
                super::ws::ResponseMode::Compact => "compact",
                super::ws::ResponseMode::LocalSummary => "local_summary",
            },
            "raw_events": self.raw_events,
            "received_events": self.received_events,
            "received_bytes": self.received_bytes,
            "prefix_truncated": self.truncated,
            "last_received_event": self.last_received_event,
            "last_received_event_index": self.truncated.then_some(self.received_events),
            "last_event_truncated": self.last_event_truncated,
            "decode_error": self.decode_error,
            "accepted_terminal": result.is_ok(),
            "capture_complete": result.is_ok() && !self.truncated,
            "outcome": match result {
                Ok(_) => "accepted_terminal",
                Err(error) if matches!(error.root_error(), LlmError::Canceled) => "canceled",
                Err(_) => "error",
            },
        });
        if let Err(error) = result {
            let mut detail = ErrorDetail::default();
            // Raw error chains stay private, not in the public error
            // projection.
            let _ = std::fmt::write(&mut detail, format_args!("{error:?}"));
            metadata["error"] = json!(detail.text);
            metadata["error_truncated"] = json!(detail.truncated);
        }
        if let Some(correlation) = correlation {
            metadata["logical_attempt"] = correlation.logical_attempt().into();
            metadata["wire_dispatch_index"] = json!(
                (correlation.wire_dispatch_index() > 0)
                    .then_some(correlation.wire_dispatch_index())
            );
            metadata["attempt_id"] = json!(correlation.diagnostic.as_ref().map(|d| d.id));
        }
        match serde_json::to_vec(&metadata) {
            Ok(json) => submit(ProviderDebugCapture::new(
                request.session_id.clone(),
                prompt_id,
                ProviderDebugCaptureClass::WebsocketResponse,
                json,
            )),
            Err(error) => {
                tracing::warn!(target: crate::LOG_TARGET, %error, "failed to serialize received response capture");
            }
        }
    }
}

/// Bounded formatting destination for internal error chains.
#[derive(Default)]
struct ErrorDetail {
    /// Exact UTF-8 prefix of the internal Debug representation.
    text: String,
    /// Whether formatting stopped at the byte ceiling.
    truncated: bool,
}

impl std::fmt::Write for ErrorDetail {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let available = ERROR_DETAIL_BYTES.saturating_sub(self.text.len());
        let end = text.floor_char_boundary(available.min(text.len()));
        self.text.push_str(&text[..end]);
        if end != text.len() {
            self.truncated = true;
            return Err(std::fmt::Error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
