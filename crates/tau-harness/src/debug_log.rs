//! [`DebugEventLog`]: append-only JSONL log of every harness event for
//! offline inspection.

use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(test)]
use test_io::{AppendFault, FaultInjectingFile};

const DEBUG_STRING_COMPACT_THRESHOLD: usize = 100;
const DEBUG_STRING_COMPACT_EDGE_BYTES: usize = 20;
const DEBUG_LOG_DIAGNOSTIC_CHARS: usize = 256;

use tau_proto::{ConnectionId, Event, UnixMicros};

use crate::error::HarnessError;
use crate::event::HarnessEvent;

/// Append-only JSON event log for debugging.
pub(crate) struct DebugEventLog {
    /// Path of the JSONL file exposed to diagnostics and tests.
    path: PathBuf,
    /// Append-open JSONL file.
    file: std::fs::File,
    /// Whether an uncertain rollback permanently disabled this process's
    /// writer.
    poisoned: bool,
    /// Deterministic one-shot append fault used by debug-log tests.
    #[cfg(test)]
    fault: Option<AppendFault>,
}

impl DebugEventLog {
    /// Opens the session's append-only debug event log.
    pub(crate) fn open(dir: &Path) -> Result<Self, HarnessError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("events.jsonl");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            file,
            poisoned: false,
            #[cfg(test)]
            fault: None,
        })
    }

    /// Returns the debug JSONL path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Logs one eligible raw harness event without changing event semantics on
    /// failure.
    pub(crate) fn log_harness_event(
        &mut self,
        harness_event: &HarnessEvent,
    ) -> Result<(), DebugLogError> {
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
                        if !emit.event.defaults_to_persist() {
                            return Ok(());
                        }
                        emit.event.name().to_string()
                    }
                    tau_proto::HarnessInputMessage::UiDebugEventStatsRequest(_) => return Ok(()),
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
            HarnessEvent::SupervisedWriterCleanupComplete { .. } | HarnessEvent::Command(_) => {
                return Ok(());
            }
        };
        self.write_entry(&entry)
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
    ) -> Result<(), DebugLogError> {
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
        self.write_entry(&entry)
    }

    /// Serializes one complete line before touching the file, then appends it
    /// failure-atomically under the debug log's existing [`Write::flush`]
    /// policy.
    fn write_entry(&mut self, entry: &serde_json::Value) -> Result<(), DebugLogError> {
        if self.poisoned {
            return Err(DebugLogError::Disabled);
        }
        let mut line = serde_json::to_vec(entry).map_err(DebugLogError::Serialize)?;
        line.push(b'\n');

        #[cfg(test)]
        let result = if let Some(fault) = self.fault.take() {
            append_line(
                &mut FaultInjectingFile::new(&mut self.file, fault, line.len()),
                &line,
            )
        } else {
            append_line(&mut self.file, &line)
        };
        #[cfg(not(test))]
        let result = append_line(&mut self.file, &line);

        result.map_err(|error| {
            if error.rollback.is_some() {
                self.poisoned = true;
            }
            DebugLogError::Append {
                source: error.source,
                rollback: error.rollback,
            }
        })
    }

    /// Installs one deterministic failure for the next line append.
    #[cfg(test)]
    fn inject_fault(&mut self, fault: AppendFault) {
        self.fault = Some(fault);
    }

    /// Makes the next append leave an uncertain rollback for harness-lifecycle
    /// tests.
    #[cfg(test)]
    pub(crate) fn inject_rollback_failure(&mut self) {
        self.inject_fault(AppendFault {
            fail_write_at: Some(1),
            fail_truncate: true,
            ..AppendFault::default()
        });
    }
}

/// An internally observable debug-log failure.
#[derive(Debug)]
pub(crate) enum DebugLogError {
    /// JSON serialization failed before the log file was touched.
    Serialize(serde_json::Error),
    /// The append failed, with rollback uncertainty when present.
    Append {
        /// Original write or commit-flush error.
        source: io::Error,
        /// Truncation or rollback-flush error that poisoned the writer.
        rollback: Option<io::Error>,
    },
    /// An earlier uncertain rollback disabled this process's writer.
    Disabled,
}

impl DebugLogError {
    /// Returns whether this failure should emit a harness diagnostic.
    ///
    /// The append that poisons the writer reports its failure. Later disabled
    /// attempts stay silent, so rollback uncertainty emits exactly one
    /// diagnostic.
    pub(crate) fn should_report(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Returns whether this failure makes the append boundary uncertain.
    pub(crate) fn disables_logging(&self) -> bool {
        matches!(
            self,
            Self::Append {
                rollback: Some(_),
                ..
            } | Self::Disabled
        )
    }

    /// Renders a character-bounded, content-free diagnostic.
    pub(crate) fn bounded_diagnostic(&self) -> String {
        let message = self.to_string();
        let mut bounded = message
            .chars()
            .take(DEBUG_LOG_DIAGNOSTIC_CHARS)
            .collect::<String>();
        if message.chars().nth(DEBUG_LOG_DIAGNOSTIC_CHARS).is_some() {
            bounded.push('…');
        }
        bounded
    }
}

impl std::fmt::Display for DebugLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialize(source) => write!(f, "failed to serialize debug JSONL entry: {source}"),
            Self::Append {
                source,
                rollback: None,
            } => write!(f, "failed to append debug JSONL entry: {source}"),
            Self::Append {
                source,
                rollback: Some(rollback),
            } => write!(
                f,
                "failed to append debug JSONL entry: {source}; rollback failed: {rollback}"
            ),
            Self::Disabled => {
                f.write_str("debug JSONL append disabled after an incomplete rollback")
            }
        }
    }
}

impl std::error::Error for DebugLogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialize(source) => Some(source),
            Self::Append { source, .. } => Some(source),
            Self::Disabled => None,
        }
    }
}

/// Minimal operations needed to append and roll back one JSONL line.
trait LineIo {
    /// Returns the exact current EOF.
    fn seek_to_end(&mut self) -> io::Result<u64>;

    /// Writes every line byte or returns the first failure.
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Truncates the log to the supplied byte offset.
    fn truncate(&mut self, offset: u64) -> io::Result<()>;

    /// Applies the debug log's existing [`Write::flush`] policy.
    fn flush(&mut self) -> io::Result<()>;
}

impl LineIo for std::fs::File {
    fn seek_to_end(&mut self) -> io::Result<u64> {
        self.seek(SeekFrom::End(0))
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        Write::write_all(self, bytes)
    }

    fn truncate(&mut self, offset: u64) -> io::Result<()> {
        self.set_len(offset)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(self)
    }
}

/// One failed line append and the status of its rollback.
#[derive(Debug)]
struct LineAppendError {
    /// Original write or commit-flush error.
    source: io::Error,
    /// Rollback failure, when future append position is uncertain.
    rollback: Option<io::Error>,
}

/// Appends and flushes one complete line, rolling back every failed mutation.
fn append_line(io: &mut impl LineIo, line: &[u8]) -> Result<(), LineAppendError> {
    let start_offset = io.seek_to_end().map_err(|source| LineAppendError {
        source,
        rollback: None,
    })?;
    let append_result = io.write_all(line).and_then(|()| io.flush());
    if let Err(source) = append_result {
        let truncate_error = io.truncate(start_offset).err();
        let rollback_flush_error = io.flush().err();
        return Err(LineAppendError {
            source,
            rollback: truncate_error.or(rollback_flush_error),
        });
    }
    Ok(())
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
        Event::ToolResultReported(result)
        | Event::ToolResult(result)
        | Event::ProviderToolResult(result) => {
            for part in &mut result.provider_content {
                let tau_proto::ToolResultContentPart::Image(image) = part;
                image.data = std::sync::Arc::from([]);
            }
        }
        Event::AgentPromptCreated(prompt) => prompt.context.clear_provider_image_bytes(),
        Event::AgentCompacted(compacted) => {
            tau_proto::clear_context_items_provider_image_bytes(&mut compacted.replacement_window);
        }
        Event::ProviderResponseFinishedReported(finished)
        | Event::ProviderResponseFinished(finished) => {
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
mod test_io;
#[cfg(test)]
mod tests;
