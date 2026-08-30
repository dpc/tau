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
    fn as_str(self) -> &'static str {
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
    fn as_str(self) -> &'static str {
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
}

impl AttemptTrace {
    /// Select the enabled trace path once without doing observation work when
    /// the dedicated target is disabled.
    #[must_use]
    pub fn selected(backend: Backend, transport: Transport) -> Option<Self> {
        tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE)
            .then(|| Self::new(backend, transport))
    }

    /// Lazily derive transport only after the dedicated target is enabled.
    #[must_use]
    pub fn selected_with(backend: Backend, transport: impl FnOnce() -> Transport) -> Option<Self> {
        tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE)
            .then(|| Self::new(backend, transport()))
    }

    /// Start enabled-only observation state.
    fn new(backend: Backend, transport: Transport) -> Self {
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

    /// Record the first decoded body chunk or WebSocket frame only.
    pub fn first_input(&mut self, bytes: usize) {
        if self.first_input_seen {
            return;
        }
        self.first_input_seen = true;
        self.first_input_us = micros(self.started_at.elapsed());
        self.first_input_bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
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
    }

    /// Record semantic qualification at a backend callback that does not own
    /// decoder timing.
    pub fn semantic_qualified(&mut self) {
        if self.first_semantic_seen {
            return;
        }
        self.first_semantic_seen = true;
        self.semantic_qualification_us = micros(self.started_at.elapsed());
    }

    /// Close and emit one finite attempt.
    pub fn finish(mut self, outcome: Outcome) {
        self.emit(outcome);
    }

    /// Emit the fixed-cardinality scalar schema.
    fn emit(&mut self, outcome: Outcome) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        self.connect_upgrade_closed();
        self.enqueue_closed();
        let total_us = micros(self.started_at.elapsed());
        let stage_accounted_us = [
            self.lowering_us,
            self.serialization_us,
            self.capture_us,
            self.pool_wait_us,
            self.connect_upgrade_us,
            self.enqueue_us,
            self.decode_us,
        ]
        .into_iter()
        .fold(0_u64, u64::saturating_add);
        tracing::trace!(
            target: LOG_TARGET,
            backend = self.backend.as_str(),
            transport = self.transport.as_str(),
            lowering_us = self.lowering_us,
            serialization_us = self.serialization_us,
            capture_us = self.capture_us,
            pool_wait_us = self.pool_wait_us,
            connect_upgrade_us = self.connect_upgrade_us,
            enqueue_us = self.enqueue_us,
            first_input_us = self.first_input_us,
            decode_us = self.decode_us,
            first_semantic_us = self.semantic_qualification_us,
            request_bytes_total = self.request_bytes_total,
            first_input_bytes = self.first_input_bytes,
            dispatch_count = self.dispatch_count,
            decode_count = self.decode_count,
            first_input_seen = self.first_input_seen,
            first_semantic_seen = self.first_semantic_seen,
            stage_accounted_us,
            unattributed_us = total_us.saturating_sub(stage_accounted_us),
            total_us,
            outcome = outcome.as_str(),
            "provider backend stage observation"
        );
    }
}

impl Drop for AttemptTrace {
    /// Close otherwise-abandoned paths without retaining their error.
    fn drop(&mut self) {
        self.emit(Outcome::Failed);
    }
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
