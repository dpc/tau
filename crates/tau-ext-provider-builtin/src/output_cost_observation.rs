//! Disabled-by-default, process-local provider output-cost observations.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
thread_local! {
    static DIAGNOSTIC_TRAVERSALS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Dedicated private trace target for provider output costs.
pub(crate) const TRACE_TARGET: &str = "provider-builtin.output-cost";

static NEXT_PROVIDER_CORRELATION: AtomicU64 = AtomicU64::new(1);

/// Enabled-only sampler materialization state transferred to the worker sink.
pub(crate) struct SamplerObservation {
    /// Process-local provider correlation shared with worker queue ownership.
    correlation: u64,
    /// Start of actual delta/event materialization.
    started: Instant,
    /// Materialized display item count.
    item_count: usize,
    /// Materialized display text bytes.
    item_bytes: usize,
    /// Whether terminal sampling requested publication.
    terminal: bool,
    /// Closed sampler materialization duration.
    materialize_elapsed: Duration,
}

impl SamplerObservation {
    /// Start only when the dedicated target is enabled.
    pub(crate) fn enabled(terminal: bool) -> Option<Self> {
        tracing::enabled!(target: TRACE_TARGET, tracing::Level::TRACE).then(|| Self {
            correlation: next_correlation(),
            started: Instant::now(),
            item_count: 0,
            item_bytes: 0,
            terminal,
            materialize_elapsed: Duration::ZERO,
        })
    }

    /// Traverse already-materialized deltas only for an enabled observation.
    pub(crate) fn count_deltas(&mut self, deltas: &[tau_proto::ProviderResponseTextDelta]) {
        self.materialize_elapsed = self.started.elapsed();
        #[cfg(test)]
        DIAGNOSTIC_TRAVERSALS.set(DIAGNOSTIC_TRAVERSALS.get().saturating_add(1));
        self.item_count = deltas.len();
        self.item_bytes = deltas
            .iter()
            .map(|delta| match delta {
                tau_proto::ProviderResponseTextDelta::Message { text, .. }
                | tau_proto::ProviderResponseTextDelta::ReasoningText { text, .. } => text.len(),
            })
            .fold(0_usize, usize::saturating_add);
    }

    /// Emit the closed sampler phase linked to the client observation when one
    /// exists.
    pub(crate) fn finish(self, outcome: &'static str) -> u64 {
        let correlation = self.correlation;
        tracing::trace!(
            target: TRACE_TARGET,
            phase = "sampler_materialization",
            provider_correlation = correlation,
            item_count = saturating_u64(self.item_count),
            item_bytes = saturating_u64(self.item_bytes),
            materialize_us = micros(self.materialize_elapsed),
            terminal = self.terminal,
            outcome,
            "provider output cost observation"
        );
        correlation
    }
}

/// Reset the disabled-path traversal oracle for the current test thread.
#[cfg(test)]
pub(crate) fn reset_diagnostic_traversals() {
    DIAGNOSTIC_TRAVERSALS.set(0);
}

/// Return enabled-only traversals on the current test thread.
#[cfg(test)]
pub(crate) fn diagnostic_traversals() -> usize {
    DIAGNOSTIC_TRAVERSALS.get()
}

/// Enabled-only exact logical queue ownership shared by producers and receiver.
pub(crate) struct WorkerQueueState {
    /// Serializes producer admission and depth snapshots.
    pub(crate) admission: Mutex<()>,
    /// Number of successfully admitted outputs not yet drained or dropped.
    depth: Mutex<usize>,
}

impl WorkerQueueState {
    /// Allocate queue accounting only while the dedicated target is enabled.
    pub(crate) fn enabled() -> Option<Arc<Self>> {
        tracing::enabled!(target: TRACE_TARGET, tracing::Level::TRACE).then(|| {
            Arc::new(Self {
                admission: Mutex::new(()),
                depth: Mutex::new(0),
            })
        })
    }
}

/// Enabled-only queue timing carried with one worker output.
pub(crate) struct WorkerOutputObservation {
    /// Admission facts published after the channel send succeeds.
    ticket: Arc<WorkerQueueTicket>,
    /// Process-local provider correlation shared across applicable phases.
    provider_correlation: u64,
    /// Start of typed frame measurement.
    started: Instant,
    /// Exact measured encoded frame bytes.
    frame_bytes: u64,
    /// Whether terminal output was emitted.
    emitted: bool,
}

/// Producer/receiver synchronization for exact queue admission facts.
struct WorkerQueueTicket {
    /// Shared exact logical queue-depth owner.
    state: Arc<WorkerQueueState>,
    /// Admission result published after `Sender::send`.
    facts: Mutex<Option<QueueFacts>>,
    /// Receiver wake after admission resolves.
    resolved: Condvar,
}

/// Exact successful queue admission facts.
struct QueueFacts {
    /// Instant immediately after successful channel admission, absent on
    /// rejection.
    queued_at: Option<Instant>,
    /// Logical queue depth at that admission.
    depth: usize,
    /// Whether this ticket still owns one depth unit.
    owns_depth: bool,
}

impl WorkerOutputObservation {
    /// Reserve one serialized logical admission while holding the depth lock.
    pub(crate) fn pending(
        state: Option<&Arc<WorkerQueueState>>,
        provider_correlation: u64,
        started: Option<Instant>,
        frame_bytes: u64,
    ) -> Option<Self> {
        let (state, started) = (state?, started?);
        Some(Self {
            ticket: Arc::new(WorkerQueueTicket {
                state: Arc::clone(state),
                facts: Mutex::new(None),
                resolved: Condvar::new(),
            }),
            provider_correlation,
            started,
            frame_bytes,
            emitted: false,
        })
    }

    /// Return the producer-side admission ticket.
    pub(crate) fn admission(&self) -> WorkerQueueAdmission {
        WorkerQueueAdmission {
            ticket: Arc::clone(&self.ticket),
        }
    }

    /// Close queue ownership and emit the fixed-schema observation.
    pub(crate) fn finish(mut self, outcome: &'static str) {
        self.emit(outcome);
    }

    fn emit(&mut self, outcome: &'static str) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        let mut facts = self.ticket.facts.lock().expect("worker queue ticket lock");
        while facts.is_none() {
            facts = self
                .ticket
                .resolved
                .wait(facts)
                .expect("worker queue admission wait");
        }
        let (queued_at, queue_depth) = facts.as_mut().map_or((None, 0), |facts| {
            if facts.owns_depth {
                facts.owns_depth = false;
                let mut depth = self
                    .ticket
                    .state
                    .depth
                    .lock()
                    .expect("worker output depth lock");
                *depth = depth.saturating_sub(1);
            }
            (facts.queued_at, facts.depth)
        });
        tracing::trace!(
            target: TRACE_TARGET,
            phase = "worker_queue",
            provider_correlation = self.provider_correlation,
            frame_bytes = self.frame_bytes,
            admission_measure_us = queued_at
                .map_or(0, |queued_at| micros(queued_at.duration_since(self.started))),
            queue_depth = saturating_u64(queue_depth),
            queue_age_us = queued_at.map_or(0, |queued_at| micros(queued_at.elapsed())),
            outcome,
            "provider output cost observation"
        );
    }
}

impl Drop for WorkerOutputObservation {
    /// Balance queue depth and close every abandoned/error path.
    fn drop(&mut self) {
        self.emit("abandoned");
    }
}

/// Producer-side publisher for exact channel admission.
pub(crate) struct WorkerQueueAdmission {
    /// Shared pending admission ticket.
    ticket: Arc<WorkerQueueTicket>,
}

impl WorkerQueueAdmission {
    /// Publish successful channel admission and its exact logical depth.
    pub(crate) fn admitted(self) {
        let mut depth = self
            .ticket
            .state
            .depth
            .lock()
            .expect("worker output depth lock");
        *depth = depth.saturating_add(1);
        let facts = QueueFacts {
            queued_at: Some(Instant::now()),
            depth: *depth,
            owns_depth: true,
        };
        drop(depth);
        *self.ticket.facts.lock().expect("worker queue ticket lock") = Some(facts);
        self.ticket.resolved.notify_all();
    }

    /// Publish failed channel admission with no queue ownership.
    pub(crate) fn rejected(self) {
        *self.ticket.facts.lock().expect("worker queue ticket lock") = Some(QueueFacts {
            queued_at: None,
            depth: 0,
            owns_depth: false,
        });
        self.ticket.resolved.notify_all();
    }
}

/// Enabled-only fixed-cardinality count for one main-loop drain call.
pub(crate) struct WorkerDrainObservation {
    /// Number of all worker messages drained.
    messages: usize,
    /// Number of typed output messages drained.
    outputs: usize,
}

impl WorkerDrainObservation {
    /// Construct only when the output-cost target is active.
    pub(crate) fn enabled() -> Option<Self> {
        if !tracing::enabled!(target: TRACE_TARGET, tracing::Level::TRACE) {
            return None;
        }
        Some(Self {
            messages: 0,
            outputs: 0,
        })
    }

    /// Count one successfully received worker message.
    pub(crate) fn message(&mut self, output: bool) {
        self.messages = self.messages.saturating_add(1);
        self.outputs = self.outputs.saturating_add(usize::from(output));
    }
}

impl Drop for WorkerDrainObservation {
    /// Emit exactly one drain-batch observation per applicable call.
    fn drop(&mut self) {
        tracing::trace!(
            target: TRACE_TARGET,
            phase = "worker_drain",
            batch_size = saturating_u64(self.messages),
            output_batch_size = saturating_u64(self.outputs),
            "provider output cost observation"
        );
    }
}

/// Start one worker measurement clock only while queue observations exist.
pub(crate) fn worker_measurement_start(state: Option<&Arc<WorkerQueueState>>) -> Option<Instant> {
    state.map(|_| Instant::now())
}

/// Allocate one provider-local correlation only for enabled worker
/// observations.
pub(crate) fn next_correlation() -> u64 {
    NEXT_PROVIDER_CORRELATION.fetch_add(1, Ordering::Relaxed)
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn saturating_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
