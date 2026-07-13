//! [`DebugEventLog`]: append-only JSONL log of every harness event for
//! offline inspection.

use std::path::{Path, PathBuf};

const DEBUG_STRING_COMPACT_THRESHOLD: usize = 100;
const DEBUG_STRING_COMPACT_EDGE_BYTES: usize = 20;

use tau_proto::{ConnectionId, Event, UnixMicros};

use crate::error::HarnessError;
use crate::event::HarnessEvent;

/// Append-only JSON event log for debugging.
pub(crate) struct DebugEventLog {
    path: PathBuf,
    file: std::fs::File,
}

impl DebugEventLog {
    pub(crate) fn open(dir: &Path) -> Result<Self, HarnessError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("events.jsonl");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self { path, file })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn log_harness_event(&mut self, harness_event: &HarnessEvent) {
        // Stamped on every line — including incoming-frame and
        // lifecycle entries that aren't event-log emissions — so an
        // offline reader can compute inter-event gaps and bursts
        // across the entire harness, not just the durable subset.
        let recorded_at = UnixMicros::now().get();
        let entry = match harness_event {
            HarnessEvent::FromConnection {
                connection_id,
                message,
                ..
            } => {
                let name = match message.as_ref() {
                    tau_proto::HarnessInputMessage::Emit(emit) => {
                        if emit.event.defaults_to_transient() {
                            return;
                        }
                        emit.event.name().to_string()
                    }
                    _ => "<message>".to_owned(),
                };
                let mut redacted_message = message.as_ref().clone();
                redact_harness_input_message_binary_content(&mut redacted_message);
                let mut frame_json = serde_json::to_value(redacted_message).unwrap_or_default();
                compact_debug_json_strings(&mut frame_json);
                serde_json::json!({
                    "type": "from_connection",
                    "recorded_at_micros": recorded_at,
                    "source": connection_id,
                    "event_name": name,
                    "event": frame_json,
                })
            }
            HarnessEvent::Disconnected { connection_id } => {
                serde_json::json!({
                    "type": "disconnected",
                    "recorded_at_micros": recorded_at,
                    "source": connection_id,
                })
            }
            HarnessEvent::ReadFailed {
                connection_id,
                error,
            } => {
                serde_json::json!({
                    "type": "read_failed",
                    "recorded_at_micros": recorded_at,
                    "source": connection_id,
                    "error": error,
                })
            }
            HarnessEvent::NewClient(_) => {
                serde_json::json!({
                    "type": "new_client",
                    "recorded_at_micros": recorded_at,
                })
            }
            HarnessEvent::Command(_) => return,
        };
        self.write_entry(&entry);
    }

    /// Logs an event the harness committed (broadcast onto the bus).
    /// Captures the *enriched* payload — for `ProviderResponseFinished`
    /// that's the harness-built `token_usage` with model and running
    /// session stats, which the inbound `from_connection` line could
    /// not carry. Together with `log_harness_event`, an offline reader
    /// can correlate the raw agent emit against the enriched committed
    /// copy.
    pub(crate) fn log_published_event(
        &mut self,
        source: Option<&ConnectionId>,
        event: &Event,
        recorded_at: UnixMicros,
    ) {
        let mut event_json = debug_event_json(event);
        redact_debug_event(&mut event_json);
        compact_debug_json_strings(&mut event_json);
        let entry = serde_json::json!({
            "type": "published",
            "recorded_at_micros": recorded_at.get(),
            "source": source,
            "event_name": event.name(),
            "event": event_json,
        });
        self.write_entry(&entry);
    }

    fn write_entry(&mut self, entry: &serde_json::Value) {
        use std::io::Write;
        let _ = serde_json::to_writer(&mut self.file, entry);
        let _ = self.file.write_all(b"\n");
        let _ = self.file.flush();
    }
}

fn redact_harness_input_message_binary_content(message: &mut tau_proto::HarnessInputMessage) {
    match message {
        tau_proto::HarnessInputMessage::Emit(emit) => {
            redact_event_binary_content(&mut emit.event);
        }
        tau_proto::HarnessInputMessage::InterceptReply(reply) => {
            if let tau_proto::InterceptAction::Pass(Some(event)) = &mut reply.action {
                redact_event_binary_content(event);
            }
        }
        tau_proto::HarnessInputMessage::CompleteTransportSend(request) => {
            for part in &mut request.tool_result.provider_content {
                let tau_proto::ToolResultContentPart::Image(image) = part;
                image.data = std::sync::Arc::from([]);
            }
        }
        _ => {}
    }
}

fn debug_event_json(event: &Event) -> serde_json::Value {
    let mut redacted = event.clone();
    redact_event_binary_content(&mut redacted);
    serde_json::to_value(redacted).unwrap_or_default()
}

fn redact_event_binary_content(event: &mut Event) {
    match event {
        Event::ToolResult(result) | Event::ProviderToolResult(result) => {
            for part in &mut result.provider_content {
                let tau_proto::ToolResultContentPart::Image(image) = part;
                image.data = std::sync::Arc::from([]);
            }
        }
        Event::AgentPromptCreated(prompt) => prompt.context.clear_provider_image_bytes(),
        Event::AgentCompacted(compacted) => {
            tau_proto::clear_context_items_provider_image_bytes(&mut compacted.replacement_window);
        }
        Event::ProviderResponseFinished(finished) => {
            tau_proto::clear_context_items_provider_image_bytes(&mut finished.output_items);
        }
        _ => {}
    }
}

fn redact_debug_event(value: &mut serde_json::Value) {
    let Some(payload) = value.get_mut("payload") else {
        return;
    };
    let is_sensitive_action = payload.get("action_id").and_then(serde_json::Value::as_str)
        == Some("email.auth.google.finish");
    if !is_sensitive_action {
        return;
    }
    if let Some(raw_line) = payload.get_mut("raw_line") {
        *raw_line = serde_json::Value::String(
            "/email auth google finish <account> <redirect-url-redacted>".to_owned(),
        );
    }
    if let Some(argv) = payload
        .get_mut("argv")
        .and_then(serde_json::Value::as_array_mut)
        && 1 < argv.len()
    {
        argv.truncate(1);
        argv.push(serde_json::Value::String(
            "<redirect-url-redacted>".to_owned(),
        ));
    }
    if let Some(arguments) = payload.get_mut("arguments") {
        *arguments = serde_json::Value::String("<redacted>".to_owned());
    }
}

fn compact_debug_json_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            *s = compact_debug_string(s);
        }
        serde_json::Value::Array(values) => {
            for value in values {
                compact_debug_json_strings(value);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                compact_debug_json_strings(value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn compact_debug_string(s: &str) -> String {
    if s.len() <= DEBUG_STRING_COMPACT_THRESHOLD {
        return s.to_owned();
    }

    let mut prefix_end = DEBUG_STRING_COMPACT_EDGE_BYTES;
    while !s.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }

    let mut suffix_start = s.len() - DEBUG_STRING_COMPACT_EDGE_BYTES;
    while suffix_start < s.len() && !s.is_char_boundary(suffix_start) {
        suffix_start += 1;
    }

    format!(
        "{}┄total {}┄{}",
        &s[..prefix_end],
        s.len(),
        &s[suffix_start..]
    )
}

#[cfg(test)]
mod tests;
