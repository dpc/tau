//! Bounded operational diagnostics for one Responses WebSocket envelope.

use std::time::{Duration, Instant};

use super::super::ParsedEventDisposition;
use super::message_read_timing::{ActiveMessageReadTiming, MessageRead};
use crate::common::LlmError;

/// Bounded, content-free observations for one WebSocket envelope.
pub(super) struct EnvelopeDiagnostics {
    /// Effective application-message silence limit for this envelope.
    idle_timeout: Duration,
    /// Active receive observations, disabled after the terminal summary.
    read_timing: Option<ActiveMessageReadTiming>,
    /// Prompt attribution already present in provider operational logs.
    agent_prompt_id: String,
    /// Process-monotonic socket attribution.
    connection_epoch: Option<u64>,
    /// Envelope start used for local elapsed timings.
    started_at: Instant,
    /// Most recent accepted text-frame time.
    last_frame_at: Option<Instant>,
    /// First complete application message received and consumed by this owner.
    first_frame_read_at: Option<Instant>,
    /// Last consumed application message's reader timestamp.
    last_frame_read_at: Option<Instant>,
    /// First application message consumption time.
    first_frame_at: Option<Instant>,
    /// Maximum observed complete-message read-to-consume delay.
    max_read_to_consume: Option<Duration>,
    /// First accepted semantic event consumption time.
    first_semantic_at: Option<Instant>,
    /// Most recent parser-accepted semantic event time.
    last_semantic_at: Option<Instant>,
    /// Accepted text-frame count.
    frame_count: u64,
    /// Total accepted text-frame bytes.
    frame_bytes: u64,
    /// Smallest accepted text-frame length.
    min_frame_bytes: Option<u64>,
    /// Largest accepted text-frame length.
    max_frame_bytes: Option<u64>,
    /// Parser-recognized event count, including semantic events.
    recognized_event_count: u64,
    /// Parser-ignored event count without provider-controlled type strings.
    unknown_event_count: u64,
    /// Parser-accepted semantic event count.
    semantic_event_count: u64,
    /// Closed local terminal reason.
    outcome: Outcome,
}

impl EnvelopeDiagnostics {
    /// Starts one bounded envelope observation.
    pub(super) fn new(
        agent_prompt_id: &str,
        connection_epoch: Option<u64>,
        idle_timeout: Duration,
        read_timing: Option<ActiveMessageReadTiming>,
    ) -> Self {
        Self {
            idle_timeout,
            read_timing,
            agent_prompt_id: agent_prompt_id.to_owned(),
            connection_epoch,
            started_at: Instant::now(),
            last_frame_at: None,
            first_frame_read_at: None,
            last_frame_read_at: None,
            first_frame_at: None,
            max_read_to_consume: None,
            first_semantic_at: None,
            last_semantic_at: None,
            frame_count: 0,
            frame_bytes: 0,
            min_frame_bytes: None,
            max_frame_bytes: None,
            recognized_event_count: 0,
            unknown_event_count: 0,
            semantic_event_count: 0,
            outcome: Outcome::ProcessingFailure,
        }
    }

    /// Records raw text-frame liveness before semantic parsing.
    pub(super) fn record_frame(
        &mut self,
        frame_bytes: usize,
        read_at: Option<Instant>,
        now: Instant,
    ) {
        self.first_frame_at.get_or_insert(now);
        if let Some(read_at) = read_at {
            self.first_frame_read_at.get_or_insert(read_at);
            self.last_frame_read_at = Some(read_at);
            let delay = now.saturating_duration_since(read_at);
            self.max_read_to_consume =
                Some(self.max_read_to_consume.unwrap_or_default().max(delay));
        }
        let frame_bytes = u64::try_from(frame_bytes).unwrap_or(u64::MAX);
        self.frame_count = self.frame_count.saturating_add(1);
        self.frame_bytes = self.frame_bytes.saturating_add(frame_bytes);
        self.min_frame_bytes = Some(
            self.min_frame_bytes
                .map_or(frame_bytes, |current| current.min(frame_bytes)),
        );
        self.max_frame_bytes = Some(
            self.max_frame_bytes
                .map_or(frame_bytes, |current| current.max(frame_bytes)),
        );
        self.last_frame_at = Some(now);
    }

    /// Reject queued receive samples from previous envelope owners.
    pub(super) fn read_at(&self, read: Option<MessageRead>) -> Option<Instant> {
        self.read_timing
            .as_ref()
            .and_then(|timing| timing.read_at(read))
    }

    /// Records the owning parser's accepted event disposition.
    pub(super) fn record_disposition(&mut self, disposition: ParsedEventDisposition) {
        match disposition {
            ParsedEventDisposition::Recognized => {
                self.recognized_event_count = self.recognized_event_count.saturating_add(1);
            }
            ParsedEventDisposition::Semantic => {
                self.recognized_event_count = self.recognized_event_count.saturating_add(1);
                self.semantic_event_count = self.semantic_event_count.saturating_add(1);
                let now = Instant::now();
                self.first_semantic_at.get_or_insert(now);
                self.last_semantic_at = Some(now);
            }
            ParsedEventDisposition::Unknown => {
                self.unknown_event_count = self.unknown_event_count.saturating_add(1);
            }
        }
    }

    /// Sets the closed terminal outcome before an explicit exit.
    pub(super) fn set_outcome(&mut self, outcome: Outcome) {
        self.outcome = outcome;
    }

    /// Emits one aggregate after the envelope reaches a terminal local outcome.
    fn log(&self) {
        let now = Instant::now();
        let controls = self.read_timing.as_ref().map(|timing| timing.controls());
        tracing::info!(
            target: crate::LOG_TARGET,
            agent_prompt_id = self.agent_prompt_id,
            connection_epoch = self.connection_epoch,
            outcome = self.outcome.label(),
            application_idle_timeout_ms = duration_millis(self.idle_timeout),
            received_ping_count = controls.map(|controls| controls.ping_count),
            received_pong_count = controls.map(|controls| controls.pong_count),
            last_received_ping_ago_ms = controls.and_then(|controls| controls.last_ping_at)
                .map(|at| duration_millis(now.saturating_duration_since(at))),
            last_received_pong_ago_ms = controls.and_then(|controls| controls.last_pong_at)
                .map(|at| duration_millis(now.saturating_duration_since(at))),
            first_application_read_ms = self.first_frame_read_at
                .map(|at| duration_millis(at.saturating_duration_since(self.started_at))),
            first_application_consumed_ms = self.first_frame_at
                .map(|at| duration_millis(at.saturating_duration_since(self.started_at))),
            last_application_read_ago_ms = self.last_frame_read_at
                .map(|at| duration_millis(now.saturating_duration_since(at))),
            max_application_read_to_consume_ms = self.max_read_to_consume.map(duration_millis),
            first_semantic_ms = self.first_semantic_at
                .map(|at| duration_millis(at.saturating_duration_since(self.started_at))),
            elapsed_ms = duration_millis(now.saturating_duration_since(self.started_at)),
            frame_count = self.frame_count,
            frame_bytes = self.frame_bytes,
            min_frame_bytes = self.min_frame_bytes,
            max_frame_bytes = self.max_frame_bytes,
            recognized_event_count = self.recognized_event_count,
            unknown_event_count = self.unknown_event_count,
            semantic_event_count = self.semantic_event_count,
            last_frame_ago_ms = self
                .last_frame_at
                .map(|at| duration_millis(now.saturating_duration_since(at))),
            last_semantic_ago_ms = self
                .last_semantic_at
                .map(|at| duration_millis(now.saturating_duration_since(at))),
            "Codex WS envelope ended",
        );
    }
}

impl Drop for EnvelopeDiagnostics {
    fn drop(&mut self) {
        self.log();
    }
}

/// Closed terminal reason for one WebSocket envelope observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Outcome {
    /// An error escaped a parser or recording helper without a narrower class.
    ProcessingFailure,
    /// Cancellation won before or during request dispatch.
    Canceled,
    /// Request serialization failed before writer handoff.
    SerializationFailure,
    /// The writer channel rejected the serialized request.
    RequestSendFailure,
    /// A prewarm absolute response deadline elapsed.
    AbsoluteTimeout,
    /// The asynchronous writer reported a send or control-ping failure.
    WriterFailure,
    /// No text frame arrived before the response idle deadline.
    ResponseIdleTimeout,
    /// The reader task disappeared without a typed terminal event.
    ReaderTaskGone,
    /// The response exceeded a bounded resource ceiling.
    ResponseResourceLimit,
    /// A text frame was not valid Responses JSON.
    MalformedText,
    /// A provider terminal event completed successfully.
    Completed,
    /// The peer closed the WebSocket or reached clean EOF.
    WebSocketClosed,
    /// A non-text or otherwise rejected frame ended the turn.
    FrameFailure,
    /// The asynchronous reader reported a transport failure.
    ReaderFailure,
}

impl Outcome {
    /// Classifies a failed request-build/dispatch helper without provider
    /// prose.
    pub(super) fn dispatch_error(error: &LlmError, dispatch_attempted: bool) -> Self {
        if dispatch_attempted {
            Self::RequestSendFailure
        } else if matches!(error.root_error(), LlmError::Canceled) {
            Self::Canceled
        } else if matches!(error.root_error(), LlmError::Json(_)) {
            Self::SerializationFailure
        } else {
            Self::ProcessingFailure
        }
    }

    /// Returns the fixed trace spelling for this outcome.
    fn label(self) -> &'static str {
        match self {
            Self::ProcessingFailure => "processing_failure",
            Self::Canceled => "canceled",
            Self::SerializationFailure => "serialization_failure",
            Self::RequestSendFailure => "request_send_failure",
            Self::AbsoluteTimeout => "absolute_timeout",
            Self::WriterFailure => "writer_failure",
            Self::ResponseIdleTimeout => "response_idle_timeout",
            Self::ReaderTaskGone => "reader_task_gone",
            Self::ResponseResourceLimit => "response_resource_limit",
            Self::MalformedText => "malformed_text",
            Self::Completed => "completed",
            Self::WebSocketClosed => "websocket_closed",
            Self::FrameFailure => "frame_failure",
            Self::ReaderFailure => "reader_failure",
        }
    }
}

/// Converts a duration to a saturating scalar tracing field.
fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
