//! Private, content-free timing observations for provider wire attempts.

use std::time::{Duration, Instant};

/// Dedicated target that operators can enable without enabling provider logs.
pub const LOG_TARGET: &str = "provider.backend-stages";

/// Closed backend family used by the private trace.
#[derive(Clone, Copy, Debug)]
pub enum Backend {
    /// OpenAI-compatible Chat Completions.
    ChatCompletions,
    /// Public OpenAI-compatible Responses.
    PublicResponses,
    /// First-party Codex Responses.
    Codex,
}

impl Backend {
    /// Return the fixed trace spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::PublicResponses => "public_responses",
            Self::Codex => "codex",
        }
    }
}

/// Closed transport family used by the private trace.
#[derive(Clone, Copy, Debug)]
pub enum Transport {
    /// HTTP with an SSE response body.
    HttpSse,
    /// WebSocket request and response frames.
    Websocket,
    /// Unary HTTP response.
    HttpUnary,
}

impl Transport {
    /// Return the fixed trace spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HttpSse => "http_sse",
            Self::Websocket => "websocket",
            Self::HttpUnary => "http_unary",
        }
    }
}

/// Closed terminal class for one finite wire attempt.
#[derive(Clone, Copy, Debug)]
pub enum Outcome {
    /// The finite attempt completed successfully.
    Completed,
    /// The attempt returned to its retry scheduler.
    Retryable,
    /// Cancellation won the attempt.
    Canceled,
    /// The attempt ended in another bounded failure class.
    Failed,
}

impl Outcome {
    /// Return the fixed trace spelling.
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Retryable => "retryable",
            Self::Canceled => "canceled",
            Self::Failed => "failed",
        }
    }
}

/// Enabled-only scalar state for one finite backend attempt.
///
/// This deliberately has no payload, identifier, URL, model, account,
/// credential, path, status body, or error fields.
#[derive(Debug)]
pub struct AttemptTrace {
    /// Closed adapter family.
    backend: Backend,
    /// Closed wire transport family.
    transport: Transport,
    /// Monotonic attempt origin.
    started_at: Instant,
    /// Monotonic lowering origin.
    lowering_started_at: Instant,
    /// Request lowering duration.
    lowering_us: u64,
    /// Aggregate serialization duration.
    serialization_us: u64,
    /// Aggregate private-capture duration.
    capture_us: u64,
    /// Aggregate backend-pool wait duration.
    pool_wait_us: u64,
    /// Aggregate fresh connect and upgrade duration.
    connect_upgrade_us: u64,
    /// Open fresh connect/upgrade stage, when owned by the caller.
    connect_upgrade_started_at: Option<Instant>,
    /// Aggregate send or enqueue duration.
    enqueue_us: u64,
    /// Open enqueue/direct-send stage, when owned by the caller.
    enqueue_started_at: Option<Instant>,
    /// Attempt-relative first input duration.
    first_input_us: u64,
    /// Aggregate decoder duration.
    decode_us: u64,
    /// Attempt-relative first semantic qualification duration.
    semantic_qualification_us: u64,
    /// Saturating total of existing serialized request buffer sizes.
    request_bytes_total: u64,
    /// First decoded body chunk or text-frame size.
    first_input_bytes: u64,
    /// Number of observed wire dispatches.
    dispatch_count: u32,
    /// Number of measured decoder invocations.
    decode_count: u32,
    /// Whether a first body chunk or text frame was observed.
    first_input_seen: bool,
    /// Whether typed semantic output qualified.
    first_semantic_seen: bool,
    /// Whether the terminal trace was already emitted.
    emitted: bool,
    /// Whether the identity-free TRACE record was selected.
    trace_enabled: bool,
    /// Response-relative observations for the current final dispatch.
    final_dispatch: FinalDispatchTiming,
}

/// Fixed first-seen observations reset whenever transparent repair dispatches.
#[derive(Debug)]
struct FinalDispatchTiming {
    /// First selected text-message read and its delay to owner dequeue.
    text_message_read: Option<(u64, u64)>,
    /// Read offset and association delay for one associated message.
    associated_message_read: Option<(u64, u64)>,
    /// First successfully decoded payload, including non-response events.
    first_decoded_payload_us: Option<u64>,
    /// Final dispatch origin for response-relative milestones.
    started_at: Option<Instant>,
    /// First owner-dequeued input after the final dispatch.
    first_input_us: Option<u64>,
    /// Size of the first owner-dequeued input after the final dispatch.
    first_input_bytes: u64,
    /// First semantic qualification after the final dispatch.
    first_semantic_us: Option<u64>,
    /// Closed connection acquisition result for the final dispatch.
    connection_state: &'static str,
    /// First response-associated event after the final dispatch.
    first_associated_event_us: Option<u64>,
    /// First nonempty text delta after the final dispatch.
    first_text_delta_us: Option<u64>,
    /// First nonempty reasoning delta after the final dispatch.
    first_reasoning_delta_us: Option<u64>,
    /// First completed actionable item after the final dispatch.
    first_actionable_item_us: Option<u64>,
    /// Terminal event after the final dispatch.
    terminal_us: Option<u64>,
}

impl Default for FinalDispatchTiming {
    fn default() -> Self {
        Self {
            text_message_read: None,
            associated_message_read: None,
            first_decoded_payload_us: None,
            started_at: None,
            first_input_us: None,
            first_input_bytes: 0,
            first_semantic_us: None,
            connection_state: "unknown",
            first_associated_event_us: None,
            first_text_delta_us: None,
            first_reasoning_delta_us: None,
            first_actionable_item_us: None,
            terminal_us: None,
        }
    }
}

impl AttemptTrace {
    /// Select the enabled trace path once without doing observation work when
    /// the dedicated target is disabled.
    #[must_use]
    pub fn selected(backend: Backend, transport: Transport) -> Option<Self> {
        tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE)
            .then(|| Self::new(backend, transport, true))
    }

    /// Select observation state for either TRACE output or eligible capture.
    #[must_use]
    pub fn selected_for_capture(
        backend: Backend,
        transport: Transport,
        capture_enabled: bool,
    ) -> Option<Self> {
        let trace_enabled = tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE);
        (trace_enabled || capture_enabled).then(|| Self::new(backend, transport, trace_enabled))
    }

    /// Lazily derive transport only after the dedicated target is enabled.
    #[must_use]
    pub fn selected_with(backend: Backend, transport: impl FnOnce() -> Transport) -> Option<Self> {
        tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE)
            .then(|| Self::new(backend, transport(), true))
    }

    /// Start enabled-only observation state.
    fn new(backend: Backend, transport: Transport, trace_enabled: bool) -> Self {
        let now = Instant::now();
        Self {
            backend,
            transport,
            started_at: now,
            lowering_started_at: now,
            lowering_us: 0,
            serialization_us: 0,
            capture_us: 0,
            pool_wait_us: 0,
            connect_upgrade_us: 0,
            connect_upgrade_started_at: None,
            enqueue_us: 0,
            enqueue_started_at: None,
            first_input_us: 0,
            decode_us: 0,
            semantic_qualification_us: 0,
            request_bytes_total: 0,
            first_input_bytes: 0,
            dispatch_count: 0,
            decode_count: 0,
            first_input_seen: false,
            first_semantic_seen: false,
            emitted: false,
            trace_enabled,
            final_dispatch: FinalDispatchTiming::default(),
        }
    }

    /// Measure request lowering without invoking the clock closure on the plain
    /// path.
    pub fn lowering_finished(&mut self) {
        self.lowering_us = micros(self.lowering_started_at.elapsed());
    }

    /// Measure lowering from an adapter-owned start boundary after unrelated
    /// pool or connection work.
    pub fn lowering_finished_from(&mut self, started_at: Instant) {
        self.lowering_us = self
            .lowering_us
            .saturating_add(micros(started_at.elapsed()));
    }

    /// Measure one serialization operation and its already-materialized size.
    pub fn serialization_finished(&mut self, started_at: Instant, bytes: usize) {
        self.serialization_us = self
            .serialization_us
            .saturating_add(micros(started_at.elapsed()));
        self.request_bytes_total = self
            .request_bytes_total
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    /// Measure private capture work.
    pub fn capture_finished(&mut self, started_at: Instant) {
        self.capture_us = self.capture_us.saturating_add(micros(started_at.elapsed()));
    }

    /// Measure a pool reservation wait.
    pub fn pool_wait_finished(&mut self, started_at: Instant) {
        self.pool_wait_us = self
            .pool_wait_us
            .saturating_add(micros(started_at.elapsed()));
    }

    /// Measure a fresh connection and protocol upgrade.
    pub fn connect_upgrade_finished(&mut self, started_at: Instant) {
        self.connect_upgrade_us = self
            .connect_upgrade_us
            .saturating_add(micros(started_at.elapsed()));
    }

    /// Open a connect/upgrade stage that terminal emission closes on failure.
    pub fn connect_upgrade_started(&mut self) {
        self.connect_upgrade_started_at = Some(Instant::now());
    }

    /// Close an open connect/upgrade stage.
    pub fn connect_upgrade_closed(&mut self) {
        if let Some(started) = self.connect_upgrade_started_at.take() {
            self.connect_upgrade_finished(started);
        }
    }

    /// Record one wire-dispatch boundary without claiming transport work that
    /// the caller cannot observe.
    pub fn record_dispatch(&mut self) {
        self.dispatch_count = self.dispatch_count.saturating_add(1);
        let connection_state = self.final_dispatch.connection_state;
        self.final_dispatch = FinalDispatchTiming {
            started_at: Some(Instant::now()),
            connection_state,
            ..FinalDispatchTiming::default()
        };
    }

    /// Record whether the final dispatch reused or opened a connection.
    pub fn connection_state(&mut self, state: &'static str) {
        self.final_dispatch.connection_state = state;
    }

    /// Measure an enqueue or direct socket-send operation owned by the caller.
    pub fn enqueue_finished(&mut self, started_at: Instant) {
        self.enqueue_us = self.enqueue_us.saturating_add(micros(started_at.elapsed()));
    }

    /// Open an enqueue stage that terminal emission closes on failure.
    pub fn enqueue_started(&mut self) {
        self.enqueue_started_at = Some(Instant::now());
    }

    /// Close an open enqueue stage.
    pub fn enqueue_closed(&mut self) {
        if let Some(started) = self.enqueue_started_at.take() {
            self.enqueue_finished(started);
        }
    }

    /// Record the first owner-dequeued body chunk or complete WebSocket
    /// message.
    pub fn first_input(&mut self, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        if !self.first_input_seen {
            self.first_input_seen = true;
            self.first_input_us = micros(self.started_at.elapsed());
            self.first_input_bytes = bytes;
        }
        if self.final_dispatch.first_input_us.is_none() {
            self.final_dispatch.first_input_us = Some(self.since_dispatch());
            self.final_dispatch.first_input_bytes = bytes;
        }
    }

    /// Record the first observed complete text message and same-message queue
    /// delay, without asserting that the message belongs to this response.
    pub fn text_message_read(&mut self, read_at: Instant) {
        if self.final_dispatch.text_message_read.is_none()
            && let Some(offset) = self.read_offset(read_at)
        {
            self.final_dispatch.text_message_read = Some((offset, micros(read_at.elapsed())));
        }
    }

    /// Record a selected message's read and association delay together.
    /// Association uses the adapter's existing classifier, not response-ID
    /// proof.
    pub fn associated_message_read(&mut self, read_at: Instant) {
        if self.final_dispatch.associated_message_read.is_none()
            && let Some(offset) = self.read_offset(read_at)
        {
            self.final_dispatch.associated_message_read = Some((offset, micros(read_at.elapsed())));
        }
    }

    /// Reject samples read before dispatch instead of saturating them to zero.
    fn read_offset(&self, read_at: Instant) -> Option<u64> {
        read_at
            .checked_duration_since(self.final_dispatch.started_at?)
            .map(micros)
    }

    /// Mark successful JSON payload decoding, independently of semantic
    /// acceptance.
    pub fn decoded_payload(&mut self) {
        if self.final_dispatch.started_at.is_some()
            && self.final_dispatch.first_decoded_payload_us.is_none()
        {
            self.final_dispatch.first_decoded_payload_us = Some(self.since_dispatch());
        }
    }

    /// Measure one semantic decoder invocation and its bounded qualification
    /// result.
    pub fn decoded(&mut self, started_at: Instant, qualifies: bool) {
        self.decoded_excluding(started_at, Duration::ZERO, qualifies);
    }

    /// Measure decoder work while excluding downstream callback time.
    pub fn decoded_excluding(&mut self, started_at: Instant, excluded: Duration, qualifies: bool) {
        self.decode_us = self
            .decode_us
            .saturating_add(micros(started_at.elapsed().saturating_sub(excluded)));
        self.decode_count = self.decode_count.saturating_add(1);
        if qualifies && !self.first_semantic_seen {
            self.first_semantic_seen = true;
            self.semantic_qualification_us = micros(self.started_at.elapsed());
        }
        if qualifies && self.final_dispatch.first_semantic_us.is_none() {
            self.final_dispatch.first_semantic_us = Some(self.since_dispatch());
        }
    }

    /// Record semantic qualification at a backend callback that does not own
    /// decoder timing.
    pub fn semantic_qualified(&mut self) {
        if !self.first_semantic_seen {
            self.first_semantic_seen = true;
            self.semantic_qualification_us = micros(self.started_at.elapsed());
        }
        if self.final_dispatch.first_semantic_us.is_none() {
            self.final_dispatch.first_semantic_us = Some(self.since_dispatch());
        }
    }

    /// Record the first decoded event proven to concern the final dispatch.
    pub fn associated_event(&mut self) {
        let value = self.since_dispatch();
        self.final_dispatch
            .first_associated_event_us
            .get_or_insert(value);
    }

    /// Record the first accepted nonempty text delta.
    pub fn text_delta(&mut self) {
        let value = self.since_dispatch();
        self.final_dispatch.first_text_delta_us.get_or_insert(value);
    }

    /// Record the first accepted nonempty reasoning delta.
    pub fn reasoning_delta(&mut self) {
        let value = self.since_dispatch();
        self.final_dispatch
            .first_reasoning_delta_us
            .get_or_insert(value);
    }

    /// Record the first completed item that can unblock local action.
    pub fn actionable_item(&mut self) {
        let value = self.since_dispatch();
        self.final_dispatch
            .first_actionable_item_us
            .get_or_insert(value);
    }

    /// Record provider terminal recognition before adapter finalization.
    pub fn terminal(&mut self) {
        let value = self.since_dispatch();
        self.final_dispatch.terminal_us.get_or_insert(value);
    }

    /// Close and emit one finite attempt.
    pub fn finish(mut self, outcome: Outcome) {
        self.emit(outcome);
    }

    /// Close one finite attempt and return its bounded scalar projection.
    pub fn finish_with_timing(mut self, outcome: Outcome) -> AttemptTiming {
        let timing = self.finish_values(outcome);
        self.emit_timing(&timing, outcome);
        timing
    }

    /// Emit the fixed-cardinality scalar schema.
    fn emit(&mut self, outcome: Outcome) {
        if self.emitted {
            return;
        }
        let timing = self.finish_values(outcome);
        self.emit_timing(&timing, outcome);
    }

    /// Emit the existing identity-free TRACE projection from frozen values.
    fn emit_timing(&self, timing: &AttemptTiming, outcome: Outcome) {
        if !self.trace_enabled {
            return;
        }
        let stage_accounted_us = [
            timing.lowering_us,
            timing.serialization_us,
            timing.capture_us,
            timing.pool_wait_us,
            timing.connect_upgrade_us,
            timing.enqueue_us,
            timing.decode_us,
        ]
        .into_iter()
        .fold(0_u64, u64::saturating_add);
        tracing::trace!(
            target: LOG_TARGET,
            backend = self.backend.as_str(),
            transport = self.transport.as_str(),
            lowering_us = timing.lowering_us,
            serialization_us = timing.serialization_us,
            capture_us = timing.capture_us,
            pool_wait_us = timing.pool_wait_us,
            connect_upgrade_us = timing.connect_upgrade_us,
            enqueue_us = timing.enqueue_us,
            first_input_us = self.first_input_us,
            decode_us = timing.decode_us,
            first_semantic_us = self.semantic_qualification_us,
            request_bytes_total = timing.request_bytes_total,
            first_input_bytes = self.first_input_bytes,
            dispatch_count = timing.dispatch_count,
            decode_count = timing.decode_count,
            first_input_seen = self.first_input_seen,
            first_semantic_seen = self.first_semantic_seen,
            stage_accounted_us,
            unattributed_us = timing.total_us.saturating_sub(stage_accounted_us),
            total_us = timing.total_us,
            outcome = outcome.as_str(),
            "provider backend stage observation"
        );
    }

    /// Freeze all durations once without logging or allocating.
    fn finish_values(&mut self, outcome: Outcome) -> AttemptTiming {
        if !self.emitted {
            self.emitted = true;
            self.connect_upgrade_closed();
            self.enqueue_closed();
        }
        let total_us = micros(self.started_at.elapsed());
        AttemptTiming {
            final_dispatch_us: self
                .final_dispatch
                .started_at
                .map(|started| micros(started.duration_since(self.started_at))),
            text_message_read: self.final_dispatch.text_message_read,
            associated_message_read: self.final_dispatch.associated_message_read,
            dispatch_to_first_decoded_payload_us: self.final_dispatch.first_decoded_payload_us,
            backend: self.backend.as_str(),
            transport: self.transport.as_str(),
            outcome: outcome.as_str(),
            dispatch_origin: match self.transport {
                Transport::Websocket => "ws_enqueue",
                Transport::HttpSse | Transport::HttpUnary => "http_send_start",
            },
            total_us,
            prepare_us: self.lowering_us.saturating_add(self.serialization_us),
            lowering_us: self.lowering_us,
            serialization_us: self.serialization_us,
            capture_us: self.capture_us,
            pool_wait_us: self.pool_wait_us,
            connect_upgrade_us: self.connect_upgrade_us,
            enqueue_us: self.enqueue_us,
            decode_us: self.decode_us,
            dispatch_to_first_input_us: self.final_dispatch.first_input_us,
            dispatch_to_first_associated_event_us: self.final_dispatch.first_associated_event_us,
            dispatch_to_first_text_delta_us: self.final_dispatch.first_text_delta_us,
            dispatch_to_first_reasoning_delta_us: self.final_dispatch.first_reasoning_delta_us,
            dispatch_to_first_actionable_item_us: self.final_dispatch.first_actionable_item_us,
            dispatch_to_first_semantic_us: self.final_dispatch.first_semantic_us,
            dispatch_to_terminal_us: self.final_dispatch.terminal_us,
            terminal_to_return_us: self
                .final_dispatch
                .terminal_us
                .map(|terminal| self.since_dispatch().saturating_sub(terminal)),
            request_bytes_total: self.request_bytes_total,
            first_input_bytes: self.final_dispatch.first_input_bytes,
            dispatch_count: self.dispatch_count,
            decode_count: self.decode_count,
            connection_state: self.final_dispatch.connection_state,
        }
    }

    /// Measure from the current final-dispatch boundary.
    fn since_dispatch(&self) -> u64 {
        self.final_dispatch
            .started_at
            .map_or(0, |started| micros(started.elapsed()))
    }
}

impl Drop for AttemptTrace {
    /// Close otherwise-abandoned paths without retaining their error.
    fn drop(&mut self) {
        self.emit(Outcome::Failed);
    }
}

/// Fixed-cardinality scalar timing projection for one finite attempt.
#[derive(Clone, Copy, Debug)]
pub struct AttemptTiming {
    /// Attempt entry to the final attempted dispatch, absent before dispatch.
    pub final_dispatch_us: Option<u64>,
    /// Final dispatch to selected text read, and that message's
    /// read-to-dequeue.
    pub text_message_read: Option<(u64, u64)>,
    /// Final dispatch to associated text read, and its
    /// read-to-association.
    pub associated_message_read: Option<(u64, u64)>,
    /// Final dispatch to first successful payload decode, not semantic
    /// acceptance.
    pub dispatch_to_first_decoded_payload_us: Option<u64>,
    /// Closed adapter backend.
    pub backend: &'static str,
    /// Closed transport.
    pub transport: &'static str,
    /// Closed attempt outcome.
    pub outcome: &'static str,
    /// Exact local dispatch origin used by response-relative fields.
    pub dispatch_origin: &'static str,
    /// Adapter attempt entry to return.
    pub total_us: u64,
    /// Lowering plus serialization, without claiming exhaustive exclusivity.
    pub prepare_us: u64,
    /// Request lowering time.
    pub lowering_us: u64,
    /// Request serialization time.
    pub serialization_us: u64,
    /// Existing request-capture construction/admission time.
    pub capture_us: u64,
    /// Pool acquisition wait time.
    pub pool_wait_us: u64,
    /// Combined fresh connection and protocol upgrade time.
    pub connect_upgrade_us: u64,
    /// Local enqueue or direct-send time.
    pub enqueue_us: u64,
    /// Aggregate decoder time excluding callbacks where supported.
    pub decode_us: u64,
    /// Final dispatch to first owner-dequeued body chunk or message.
    pub dispatch_to_first_input_us: Option<u64>,
    /// Final dispatch to first response-associated decoded event.
    pub dispatch_to_first_associated_event_us: Option<u64>,
    /// Final dispatch to first nonempty text delta.
    pub dispatch_to_first_text_delta_us: Option<u64>,
    /// Final dispatch to first nonempty reasoning delta.
    pub dispatch_to_first_reasoning_delta_us: Option<u64>,
    /// Final dispatch to first completed actionable item.
    pub dispatch_to_first_actionable_item_us: Option<u64>,
    /// Final dispatch to first existing semantic qualification.
    pub dispatch_to_first_semantic_us: Option<u64>,
    /// Final dispatch to provider terminal recognition.
    pub dispatch_to_terminal_us: Option<u64>,
    /// Provider terminal recognition to adapter return.
    pub terminal_to_return_us: Option<u64>,
    /// Existing serialized request byte total.
    pub request_bytes_total: u64,
    /// First owner-dequeued input size.
    pub first_input_bytes: u64,
    /// Number of actual wire dispatches.
    pub dispatch_count: u32,
    /// Number of measured decoder invocations.
    pub decode_count: u32,
    /// Closed final connection acquisition state.
    pub connection_state: &'static str,
}

/// Read the enabled-only observation clock at a call site.
#[must_use]
pub fn started(trace: &Option<AttemptTrace>) -> Option<Instant> {
    trace.as_ref().map(|_| Instant::now())
}

/// Saturating scalar projection used by every duration field.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
