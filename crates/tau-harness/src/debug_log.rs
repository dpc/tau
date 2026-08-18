//! [`DebugEventLog`]: serialization and nonblocking admission for the
//! append-only JSONL debug mirror.

#[cfg(test)]
use std::fs as path_std_fs;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync as path_std_sync;
use std::time::{Duration, Instant};

#[cfg(test)]
use test_io::{AppendFault, FaultInjectingFile};

const DEBUG_STRING_COMPACT_THRESHOLD: usize = 100;
const DEBUG_STRING_COMPACT_EDGE_BYTES: usize = 20;
const DEBUG_LOG_DIAGNOSTIC_CHARS: usize = 256;
const SLOW_DEBUG_LOG_CYCLE: Duration = Duration::from_millis(500);

use tau_proto::{ConnectionId, Event, UnixMicros};

use crate::error::HarnessError;
use crate::event::HarnessEvent;

mod writer;
#[cfg(not(test))]
use writer::enqueue;

/// Append-only JSON event log for debugging.
pub(crate) struct DebugEventLog {
    /// Path of the JSONL file exposed to diagnostics and tests.
    path: PathBuf,
    /// Synchronous append handle retained only by fault-injection tests.
    #[cfg(test)]
    file: std::fs::File,
    /// Whether an uncertain rollback permanently disabled this process's
    /// writer.
    #[cfg(test)]
    poisoned: bool,
    /// Deterministic one-shot append fault used by debug-log tests.
    #[cfg(test)]
    fault: Option<AppendFault>,
}

impl DebugEventLog {
    /// Creates a producer for the session's append-only debug event log.
    pub(crate) fn open(dir: &Path) -> Result<Self, HarnessError> {
        let path = dir.join("events.jsonl");
        #[cfg(test)]
        std::fs::create_dir_all(dir)?;
        #[cfg(test)]
        let file = path_std_fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            #[cfg(test)]
            file,
            #[cfg(test)]
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
                    tau_proto::HarnessInputMessage::ProviderDebugCapture(_) => return Ok(()),
                    _ => "<message>".to_owned(),
                };
                let mut frame_json = debug_harness_input_json(message.as_ref());
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
            HarnessEvent::SupervisedWriterCleanupComplete { .. }
            | HarnessEvent::Command(_)
            | HarnessEvent::ComponentIngressReady => {
                return Ok(());
            }
        };
        self.write_entry(&entry)
    }

    /// Observes an event at its pre-semantic-persistence publication point.
    /// Captures the *enriched* payload — for `ProviderResponseFinished`
    /// that's the harness-built `token_usage` with model and running
    /// session stats, which the inbound `from_connection` line could
    /// not carry. Together with `log_harness_event`, an offline reader
    /// can correlate the raw agent emit against the enriched attempted copy.
    /// The row may remain even when later semantic persistence rejects the
    /// event.
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

    /// Serializes one complete line before nonblocking queue admission.
    fn write_entry(&mut self, entry: &serde_json::Value) -> Result<(), DebugLogError> {
        #[cfg(test)]
        if self.poisoned {
            return Err(DebugLogError::Disabled);
        }
        let cycle_started = Instant::now();
        let serialize_started = Instant::now();
        let mut line = match serde_json::to_vec(entry) {
            Ok(line) => line,
            Err(error) => {
                let timing = DebugLogCycleTiming {
                    total: cycle_started.elapsed(),
                    serialize: serialize_started.elapsed(),
                    ..DebugLogCycleTiming::default()
                };
                timing.trace(entry, 0, DebugLogCycleResult::SerializeError);
                return Err(DebugLogError::Serialize(error));
            }
        };
        let serialize = serialize_started.elapsed();
        line.push(b'\n');

        #[cfg(not(test))]
        {
            let line_bytes = line.len();
            let accepted = enqueue(self.path.clone(), line);
            let timing = DebugLogCycleTiming {
                total: cycle_started.elapsed(),
                serialize,
                ..DebugLogCycleTiming::default()
            };
            timing.trace(
                entry,
                line_bytes,
                if accepted {
                    DebugLogCycleResult::Queued
                } else {
                    DebugLogCycleResult::Dropped
                },
            );
            Ok(())
        }

        #[cfg(test)]
        let result = if let Some(fault) = self.fault.take() {
            append_line(
                &mut FaultInjectingFile::new(&mut self.file, fault, line.len()),
                &line,
            )
        } else {
            append_line(&mut self.file, &line)
        };
        #[cfg(test)]
        let (append, result) = match result {
            Ok(append) => (append, Ok(())),
            Err(error) => {
                if error.rollback.is_some() {
                    self.poisoned = true;
                }
                let debug_error = DebugLogError::Append {
                    source: error.source,
                    rollback: error.rollback,
                };
                (error.timing, Err(debug_error))
            }
        };
        #[cfg(test)]
        let timing = DebugLogCycleTiming {
            total: cycle_started.elapsed(),
            serialize,
            append,
        };
        #[cfg(test)]
        let result_class = if result.is_ok() {
            DebugLogCycleResult::Appended
        } else {
            DebugLogCycleResult::AppendError
        };
        #[cfg(test)]
        timing.trace(entry, line.len(), result_class);
        #[cfg(test)]
        result
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
    #[cfg(test)]
    Disabled,
}

impl DebugLogError {
    /// Returns whether this failure should emit a harness diagnostic.
    ///
    /// The append that poisons the writer reports its failure. Later disabled
    /// attempts stay silent, so rollback uncertainty emits exactly one
    /// diagnostic.
    pub(crate) fn should_report(&self) -> bool {
        #[cfg(test)]
        {
            !matches!(self, Self::Disabled)
        }
        #[cfg(not(test))]
        {
            true
        }
    }

    /// Returns whether this failure makes the append boundary uncertain.
    pub(crate) fn disables_logging(&self) -> bool {
        match self {
            Self::Append { rollback, .. } => rollback.is_some(),
            #[cfg(test)]
            Self::Disabled => true,
            Self::Serialize(_) => false,
        }
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
            #[cfg(test)]
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
            #[cfg(test)]
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
    /// Phase timing and offsets observed before the failure.
    timing: LineAppendTiming,
}

impl LineAppendError {
    fn without_timing(source: io::Error) -> Self {
        Self {
            source,
            rollback: None,
            timing: LineAppendTiming::default(),
        }
    }
}

/// Content-free measurements for one append/rollback attempt.
#[derive(Clone, Copy, Debug, Default)]
struct LineAppendTiming {
    /// Time spent obtaining the exact pre-append EOF.
    eof: Duration,
    /// Time spent writing and applying the existing flush policy.
    write_flush: Duration,
    /// Time spent truncating and flushing after failure.
    rollback: Duration,
    /// Exact EOF before mutation, when it could be obtained.
    start_offset: Option<u64>,
    /// Exact expected EOF after successful append or rollback.
    end_offset: Option<u64>,
}

/// Content-free measurements for serialization plus append processing.
#[derive(Clone, Copy, Debug, Default)]
struct DebugLogCycleTiming {
    /// Whole producer serialization/admission cycle.
    total: Duration,
    /// JSON serialization phase.
    serialize: Duration,
    /// File append and possible rollback phases.
    append: LineAppendTiming,
}

/// Terminal classification for one measured debug-log cycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DebugLogCycleResult {
    /// Serialization failed before file mutation.
    SerializeError,
    /// Append, flush, or rollback processing failed.
    AppendError,
    #[cfg(not(test))]
    /// The producer admitted one serialized line.
    Queued,
    #[cfg(not(test))]
    /// The producer dropped one serialized line without waiting.
    Dropped,
    /// One complete line was appended and flushed by a file writer.
    Appended,
}

/// Content-free fields emitted for one worker I/O cycle.
#[derive(Clone, Copy, Debug)]
struct DebugLogWorkerTrace {
    /// Terminal classification of the worker attempt.
    result: DebugLogCycleResult,
    /// Whole worker cycle duration in microseconds.
    total_us: u64,
    /// Exact-EOF lookup duration in microseconds.
    eof_us: u64,
    /// Append and flush duration in microseconds.
    write_flush_us: u64,
    /// Rollback duration in microseconds.
    rollback_us: u64,
    /// Encoded line size.
    line_bytes: usize,
    /// Exact EOF before mutation, when observed.
    start_eof: Option<u64>,
    /// Expected EOF after append or rollback, when known.
    end_eof: Option<u64>,
    /// Whether the cycle exceeded the slow-cycle threshold.
    slow: bool,
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

impl DebugLogCycleTiming {
    fn trace(&self, entry: &serde_json::Value, line_bytes: usize, result: DebugLogCycleResult) {
        let event_name = entry
            .get("event_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<none>");
        let total_us = duration_micros(self.total);
        let serialize_us = duration_micros(self.serialize);
        let eof_us = duration_micros(self.append.eof);
        let write_flush_us = duration_micros(self.append.write_flush);
        let rollback_us = duration_micros(self.append.rollback);
        let start_eof = self.append.start_offset;
        let end_eof = self.append.end_offset;
        tracing::trace!(
            target: "tau_harness::debug_log_timing",
            event_name,
            ?result,
            total_us,
            serialize_us,
            eof_us,
            write_flush_us,
            rollback_us,
            line_bytes,
            ?start_eof,
            ?end_eof,
            "debug JSONL producer cycle"
        );
        if SLOW_DEBUG_LOG_CYCLE < self.total {
            tracing::warn!(
                target: "tau_harness::debug_log_timing",
                event_name,
                ?result,
                total_us,
                serialize_us,
                eof_us,
                write_flush_us,
                rollback_us,
                line_bytes,
                ?start_eof,
                ?end_eof,
                "slow debug JSONL producer cycle"
            );
        }
    }
}

impl LineAppendTiming {
    fn trace_worker(
        &self,
        total: Duration,
        line_bytes: usize,
        result: DebugLogCycleResult,
    ) -> DebugLogWorkerTrace {
        let trace = DebugLogWorkerTrace {
            result,
            total_us: duration_micros(total),
            eof_us: duration_micros(self.eof),
            write_flush_us: duration_micros(self.write_flush),
            rollback_us: duration_micros(self.rollback),
            line_bytes,
            start_eof: self.start_offset,
            end_eof: self.end_offset,
            slow: SLOW_DEBUG_LOG_CYCLE < total,
        };
        tracing::trace!(
            target: "tau_harness::debug_log_timing",
            result = ?trace.result,
            total_us = trace.total_us,
            eof_us = trace.eof_us,
            write_flush_us = trace.write_flush_us,
            rollback_us = trace.rollback_us,
            line_bytes = trace.line_bytes,
            start_eof = ?trace.start_eof,
            end_eof = ?trace.end_eof,
            "debug JSONL worker I/O cycle"
        );
        if trace.slow {
            tracing::warn!(
                target: "tau_harness::debug_log_timing",
                result = ?trace.result,
                total_us = trace.total_us,
                eof_us = trace.eof_us,
                write_flush_us = trace.write_flush_us,
                rollback_us = trace.rollback_us,
                line_bytes = trace.line_bytes,
                start_eof = ?trace.start_eof,
                end_eof = ?trace.end_eof,
                "slow debug JSONL worker I/O cycle"
            );
        }
        trace
    }
}

/// Appends and flushes one complete line, rolling back every failed mutation.
fn append_line(io: &mut impl LineIo, line: &[u8]) -> Result<LineAppendTiming, LineAppendError> {
    let mut timing = LineAppendTiming::default();
    let eof_started = Instant::now();
    let start_offset = match io.seek_to_end() {
        Ok(offset) => offset,
        Err(source) => {
            timing.eof = eof_started.elapsed();
            return Err(LineAppendError {
                source,
                rollback: None,
                timing,
            });
        }
    };
    timing.eof = eof_started.elapsed();
    timing.start_offset = Some(start_offset);
    let write_flush_started = Instant::now();
    let append_result = io.write_all(line).and_then(|()| io.flush());
    timing.write_flush = write_flush_started.elapsed();
    if let Err(source) = append_result {
        let rollback_started = Instant::now();
        let truncate_error = io.truncate(start_offset).err();
        let rollback_flush_error = io.flush().err();
        timing.rollback = rollback_started.elapsed();
        if truncate_error.is_none() && rollback_flush_error.is_none() {
            timing.end_offset = Some(start_offset);
        }
        return Err(LineAppendError {
            source,
            rollback: truncate_error.or(rollback_flush_error),
            timing,
        });
    }
    timing.end_offset = Some(start_offset.saturating_add(line.len() as u64));
    Ok(timing)
}

fn debug_harness_input_json(message: &tau_proto::HarnessInputMessage) -> serde_json::Value {
    match message {
        tau_proto::HarnessInputMessage::Emit(emit)
            if matches!(
                emit.event.as_ref(),
                Event::ProviderResponseUpdated(updated)
                    | Event::ProviderResponseUpdatedReported(updated)
                    if updated.status.as_ref().and_then(|status| status.retry.as_ref()).is_some()
            ) =>
        {
            let (Event::ProviderResponseUpdated(updated)
            | Event::ProviderResponseUpdatedReported(updated)) = emit.event.as_ref()
            else {
                unreachable!();
            };
            serde_json::json!({
                "message": "emit",
                "payload": {
                    "event": provider_retry_debug_projection(emit.event.name(), updated),
                    "persist": emit.persist,
                },
            })
        }
        tau_proto::HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::AgentPromptCreated(_)) =>
        {
            let Event::AgentPromptCreated(prompt) = emit.event.as_ref() else {
                unreachable!();
            };
            serde_json::json!({
                "message": "emit",
                "payload": {
                    "event": prompt_created_debug_summary(prompt),
                    "persist": emit.persist,
                },
            })
        }
        tau_proto::HarnessInputMessage::InterceptReply(reply) => {
            if let tau_proto::InterceptAction::Pass(Some(event)) = &reply.action
                && let Event::ProviderResponseUpdated(updated)
                | Event::ProviderResponseUpdatedReported(updated) = event.as_ref()
                && updated
                    .status
                    .as_ref()
                    .and_then(|status| status.retry.as_ref())
                    .is_some()
            {
                return serde_json::json!({
                    "message": "intercept_reply",
                    "payload": {
                        "action": {
                            "kind": "pass",
                            "value": provider_retry_debug_projection(event.name(), updated),
                        },
                    },
                });
            }
            if let tau_proto::InterceptAction::Pass(Some(event)) = &reply.action
                && let Event::AgentPromptCreated(prompt) = event.as_ref()
            {
                return serde_json::json!({
                    "message": "intercept_reply",
                    "payload": {
                        "action": {
                            "kind": "pass",
                            "value": prompt_created_debug_summary(prompt),
                        },
                    },
                });
            }
            let mut redacted = message.clone();
            redact_harness_input_message_binary_content(&mut redacted);
            serde_json::to_value(redacted).unwrap_or_default()
        }
        _ => {
            let mut redacted = message.clone();
            redact_harness_input_message_binary_content(&mut redacted);
            serde_json::to_value(redacted).unwrap_or_default()
        }
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
        _ => {}
    }
}

fn debug_event_json(event: &Event) -> serde_json::Value {
    if let Event::AgentPromptCreated(prompt) = event {
        return prompt_created_debug_summary(prompt);
    }
    if let Event::ProviderResponseUpdated(updated) | Event::ProviderResponseUpdatedReported(updated) =
        event
        && updated
            .status
            .as_ref()
            .and_then(|status| status.retry.as_ref())
            .is_some()
    {
        return provider_retry_debug_projection(event.name(), updated);
    }
    let mut redacted = event.clone();
    redact_event_binary_content(&mut redacted);
    serde_json::to_value(redacted).unwrap_or_default()
}

fn provider_retry_debug_projection(
    event_name: tau_proto::EventName,
    updated: &tau_proto::ProviderResponseUpdated,
) -> serde_json::Value {
    let status = updated
        .status
        .as_ref()
        .expect("caller selected a retry update");
    let retry = status
        .retry
        .as_ref()
        .expect("caller selected a retry update");
    serde_json::json!({
        "event": event_name,
        "payload": {
            "agent_prompt_id": updated.agent_prompt_id,
            "agent_id": updated.agent_id,
            "status": {
                "text": "retrying",
                "clear_response": status.clear_response,
                "retry": retry,
            },
            "originator": updated.originator,
        },
    })
}

fn prompt_created_debug_summary(prompt: &tau_proto::AgentPromptCreated) -> serde_json::Value {
    #[derive(Default)]
    struct Counts {
        items: u64,
        text_bytes: u64,
        images: u64,
        image_bytes: u64,
    }

    fn add_tool_result(counts: &mut Counts, result: &tau_proto::ToolResultItem) {
        for part in &result.provider_content {
            let tau_proto::ToolResultContentPart::Image(image) = part;
            counts.images = counts.images.saturating_add(1);
            counts.image_bytes = counts
                .image_bytes
                .saturating_add(u64::try_from(image.data.len()).unwrap_or(u64::MAX));
        }
    }

    fn add_item(counts: &mut Counts, item: &tau_proto::ContextItem) {
        counts.items = counts.items.saturating_add(1);
        match item {
            tau_proto::ContextItem::Message(message) => {
                for part in &message.content {
                    let (tau_proto::ContentPart::Text { text }
                    | tau_proto::ContentPart::HarnessInternalText { text }) = part;
                    counts.text_bytes = counts
                        .text_bytes
                        .saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
                }
            }
            tau_proto::ContextItem::ToolResult(result) => add_tool_result(counts, result),
            tau_proto::ContextItem::ReasoningText(reasoning) => {
                counts.text_bytes = counts
                    .text_bytes
                    .saturating_add(u64::try_from(reasoning.text.len()).unwrap_or(u64::MAX));
            }
            tau_proto::ContextItem::LocalCompactionNarrative(narrative) => {
                counts.text_bytes = counts
                    .text_bytes
                    .saturating_add(u64::try_from(narrative.narrative.len()).unwrap_or(u64::MAX));
            }
            tau_proto::ContextItem::ToolCall(_)
            | tau_proto::ContextItem::Reasoning(_)
            | tau_proto::ContextItem::CompactionTrigger
            | tau_proto::ContextItem::Compaction(_)
            | tau_proto::ContextItem::UnknownProviderItem(_) => {}
        }
    }

    let mut counts = Counts::default();
    for block in &prompt.context.blocks {
        match block {
            tau_proto::ContextBlock::UserInput(block) => {
                for item in &block.items {
                    add_item(&mut counts, item);
                }
            }
            tau_proto::ContextBlock::AssistantResponse(block) => {
                for item in &block.output_items {
                    add_item(&mut counts, item);
                }
            }
            tau_proto::ContextBlock::ToolResults(block) => {
                for result in &block.items {
                    counts.items = counts.items.saturating_add(1);
                    add_tool_result(&mut counts, result);
                }
            }
        }
    }

    serde_json::json!({
        "event": "agent.prompt_created",
        "payload": {
            "agent_prompt_id": prompt.agent_prompt_id,
            "agent_id": prompt.agent_id,
            "session_id": prompt.session_id,
            "model": prompt.model,
            "operation": prompt.operation,
            "summary": {
                "system_prompt_utf8_bytes":
                    u64::try_from(prompt.system_prompt.len()).unwrap_or(u64::MAX),
                "context_blocks":
                    u64::try_from(prompt.context.blocks.len()).unwrap_or(u64::MAX),
                "context_items": counts.items,
                "context_text_utf8_bytes": counts.text_bytes,
                "provider_images": counts.images,
                "provider_image_bytes": counts.image_bytes,
                "tools": u64::try_from(prompt.tools.len()).unwrap_or(u64::MAX),
            },
        },
    })
}

fn redact_event_binary_content(event: &mut Event) {
    match event {
        Event::ToolResultReported(result)
        | Event::ToolResult(result)
        | Event::ProviderToolResult(result) => {
            for part in &mut result.provider_content {
                let tau_proto::ToolResultContentPart::Image(image) = part;
                image.data = path_std_sync::Arc::from([]);
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
            ":email auth google finish <account> <redirect-url-redacted>".to_owned(),
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
