//! Disabled-by-default, process-local output-cost observations.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Dedicated private trace target for client output costs.
pub(crate) const TRACE_TARGET: &str = "tau_client::output_cost";

static NEXT_CORRELATION: AtomicU64 = AtomicU64::new(1);

/// Closed writer-lane classification for one output.
#[derive(Clone, Copy)]
pub(crate) enum OutputLane {
    /// Acknowledged synchronous output.
    Synchronous,
    /// Best-effort detached output.
    Detached,
}

impl OutputLane {
    /// Return the fixed trace label.
    const fn label(self) -> &'static str {
        match self {
            Self::Synchronous => "synchronous",
            Self::Detached => "detached",
        }
    }
}

/// Enabled-only admission completion handle retained by the producer.
#[derive(Clone)]
pub(crate) struct AdmissionObservation {
    /// Shared observation synchronized with the writer.
    state: Arc<ObservationState>,
}

impl AdmissionObservation {
    /// Publish successful admission at the actual queue linearization point.
    pub(crate) fn admitted(&self) {
        let mut inner = self.state.inner.lock().expect("output observation lock");
        inner.admission_elapsed = inner
            .admission_started
            .map_or(Duration::ZERO, |started| started.elapsed());
        inner.admitted_at = Some(Instant::now());
        inner.admission_done = true;
        self.state.admission.notify_all();
    }

    /// Publish a closed rejection before writer applicability begins.
    pub(crate) fn rejected(&self, outcome: &'static str) {
        let mut inner = self.state.inner.lock().expect("output observation lock");
        inner.admission_elapsed = inner
            .admission_started
            .map_or(Duration::ZERO, |started| started.elapsed());
        inner.admission_done = true;
        emit_locked(&mut inner, Duration::ZERO, Duration::ZERO, outcome);
        self.state.admission.notify_all();
    }
}

/// Synchronization shared only while diagnostics are enabled.
struct ObservationState {
    /// Mutable scalar phase state.
    inner: Mutex<Inner>,
    /// Writer wait for producer-side admission completion.
    admission: Condvar,
}

/// Fixed scalar state for one output.
struct Inner {
    /// Process-local client correlation value.
    correlation: u64,
    /// Exact measured encoded frame bytes.
    frame_bytes: u64,
    /// Counting-encoder duration.
    measure_elapsed: Duration,
    /// Start of actual local admission work.
    admission_started: Option<Instant>,
    /// Closed local admission duration.
    admission_elapsed: Duration,
    /// Actual successful admission instant.
    admitted_at: Option<Instant>,
    /// Queue/channel wait closed before encoding.
    writer_wait: Duration,
    /// Whether admission has resolved.
    admission_done: bool,
    /// Closed writer lane.
    lane: Option<OutputLane>,
    /// Whether the terminal observation was emitted.
    emitted: bool,
}

/// Enabled-only scalar timing state carried with one typed output.
pub(crate) struct OutputCostObservation {
    /// Shared producer/writer phase state.
    state: Arc<ObservationState>,
}

impl OutputCostObservation {
    /// Start an observation only when its dedicated target is enabled.
    pub(crate) fn start() -> Option<Instant> {
        tracing::enabled!(target: TRACE_TARGET, tracing::Level::TRACE).then(Instant::now)
    }

    /// Construct enabled-only state after exact measurement completes.
    pub(crate) fn measured(started: Option<Instant>, frame_bytes: u64) -> Option<Self> {
        started.map(|started| Self {
            state: Arc::new(ObservationState {
                inner: Mutex::new(Inner {
                    correlation: NEXT_CORRELATION.fetch_add(1, Ordering::Relaxed),
                    frame_bytes,
                    measure_elapsed: started.elapsed(),
                    admission_started: None,
                    admission_elapsed: Duration::ZERO,
                    admitted_at: None,
                    writer_wait: Duration::ZERO,
                    admission_done: false,
                    lane: None,
                    emitted: false,
                }),
                admission: Condvar::new(),
            }),
        })
    }

    /// Start actual local admission and return its producer completion handle.
    pub(crate) fn begin_admission(&self, lane: OutputLane) -> AdmissionObservation {
        let mut inner = self.state.inner.lock().expect("output observation lock");
        inner.lane = Some(lane);
        inner.admission_started = Some(Instant::now());
        drop(inner);
        AdmissionObservation {
            state: Arc::clone(&self.state),
        }
    }

    /// Wait for actual admission, then close writer queue/channel wait.
    pub(crate) fn writer_started(&self) -> bool {
        let mut inner = self.state.inner.lock().expect("output observation lock");
        while !inner.admission_done {
            inner = self
                .state
                .admission
                .wait(inner)
                .expect("output observation admission wait");
        }
        if inner.emitted || inner.admitted_at.is_none() {
            return false;
        }
        inner.writer_wait = inner
            .admitted_at
            .map_or(Duration::ZERO, |admitted| admitted.elapsed());
        true
    }

    /// Emit one fixed-schema terminal observation after writer work.
    pub(crate) fn finish(
        &self,
        encode_elapsed: Duration,
        flush_elapsed: Duration,
        outcome: &'static str,
    ) {
        let mut inner = self.state.inner.lock().expect("output observation lock");
        emit_locked(&mut inner, encode_elapsed, flush_elapsed, outcome);
    }
}

impl Drop for OutputCostObservation {
    /// Close outputs abandoned before a terminal producer or writer outcome.
    fn drop(&mut self) {
        if Arc::strong_count(&self.state) == 1 {
            let mut inner = self.state.inner.lock().expect("output observation lock");
            emit_locked(&mut inner, Duration::ZERO, Duration::ZERO, "abandoned");
        }
    }
}

/// Emit while holding the one observation lock.
fn emit_locked(
    inner: &mut Inner,
    encode_elapsed: Duration,
    flush_elapsed: Duration,
    outcome: &'static str,
) {
    if inner.emitted {
        return;
    }
    inner.emitted = true;
    tracing::trace!(
        target: TRACE_TARGET,
        client_correlation = inner.correlation,
        lane = inner.lane.map_or("not_applicable", OutputLane::label),
        frame_bytes = inner.frame_bytes,
        measure_us = micros(inner.measure_elapsed),
        admission_us = micros(inner.admission_elapsed),
        writer_wait_us = micros(inner.writer_wait),
        encode_us = micros(encode_elapsed),
        flush_us = micros(flush_elapsed),
        outcome,
        "client output cost observation"
    );
}

/// Saturate one duration into the fixed scalar schema.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
