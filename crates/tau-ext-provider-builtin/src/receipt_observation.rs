//! Private, content-free provider receipt-to-worker-start observations.

use std::time::{Duration, Instant};

use tau_client::LocalInputObservation;

/// Dedicated target that can be enabled without broad Provider diagnostics.
pub(crate) const RECEIPT_LOG_TARGET: &str = "provider-builtin.receipt";

/// Fixed OAuth stage class for one receipt observation.
#[derive(Clone, Copy, Debug)]
enum OAuthClass {
    /// No OAuth work was required.
    None,
    /// This prompt joined existing OAuth work.
    Joined,
    /// This prompt started OAuth work.
    Started,
    /// OAuth work produced authoritative credentials.
    Refreshed,
    /// OAuth work failed or was abandoned.
    Failed,
}

impl OAuthClass {
    /// Returns the fixed trace spelling.
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Joined => "joined",
            Self::Started => "started",
            Self::Refreshed => "refreshed",
            Self::Failed => "failed",
        }
    }
}

/// Fixed terminal class for one receipt observation.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ReceiptOutcome {
    /// A provider worker began execution.
    Started,
    /// Prompt ownership was canceled before worker start.
    Canceled,
    /// Ownership ended on another bounded pre-worker failure path.
    Failed,
}

impl ReceiptOutcome {
    /// Returns the fixed trace spelling.
    fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Canceled => "canceled",
            Self::Failed => "failed",
        }
    }
}

/// Enabled-only scalar observation carried with one prompt inside this process.
#[derive(Debug)]
pub(crate) struct ReceiptObservation {
    /// Encoded bytes consumed by the real input decoder.
    frame_bytes: u64,
    /// Blocking frame-read plus decode duration after the reader starts
    /// waiting.
    decode_elapsed: Duration,
    /// Monotonic completion instant for decode.
    decoded_at: Instant,
    /// Optional handler-entry anchor.
    handler_started_at: Option<Instant>,
    /// Event clone and handler materialization duration.
    handler_elapsed: Duration,
    /// Handler post-materialization anchor.
    handler_materialized_at: Option<Instant>,
    /// Handler dispatch duration after event materialization.
    handler_dispatch_elapsed: Duration,
    /// Selected settings clone duration.
    settings_elapsed: Duration,
    /// Number of profiles in the selected settings clone.
    profile_count: usize,
    /// Number of prompt-owned Secret RPCs started.
    secret_rpc_count: u32,
    /// Aggregate successful Secret read value bytes.
    secret_bytes: u64,
    /// Current sequential Secret RPC anchor.
    secret_started_at: Option<Instant>,
    /// Aggregate Secret RPC wait duration.
    secret_elapsed: Duration,
    /// Optional OAuth network-work anchor.
    oauth_started_at: Option<Instant>,
    /// Closed OAuth network-work duration.
    oauth_elapsed: Duration,
    /// Closed OAuth outcome class.
    oauth_class: OAuthClass,
    /// Quota resolution duration.
    quota_elapsed: Duration,
    /// Optional cooldown-queue entry anchor.
    cooldown_entered_at: Option<Instant>,
    /// Cooldown queue depth observed at entry.
    cooldown_depth: usize,
    /// Closed cooldown queue duration.
    cooldown_elapsed: Duration,
    /// Optional worker-slot queue entry anchor.
    queue_entered_at: Option<Instant>,
    /// Worker-slot queue depth observed at entry.
    queue_depth: usize,
    /// Closed worker-slot queue duration.
    queue_elapsed: Duration,
    /// Optional thread-spawn entry anchor.
    spawn_started_at: Option<Instant>,
    /// Whether the exactly-once terminal event was emitted.
    emitted: bool,
}

impl ReceiptObservation {
    /// Creates an observation from transport-owned facts gathered on the real
    /// decode path.
    pub(crate) fn new(input: LocalInputObservation) -> Self {
        Self {
            frame_bytes: input.frame_bytes.get(),
            decode_elapsed: input.decode_elapsed,
            decoded_at: input.decoded_at,
            handler_started_at: None,
            handler_elapsed: Duration::ZERO,
            handler_materialized_at: None,
            handler_dispatch_elapsed: Duration::ZERO,
            settings_elapsed: Duration::ZERO,
            profile_count: 0,
            secret_rpc_count: 0,
            secret_bytes: 0,
            secret_started_at: None,
            secret_elapsed: Duration::ZERO,
            oauth_started_at: None,
            oauth_elapsed: Duration::ZERO,
            oauth_class: OAuthClass::None,
            quota_elapsed: Duration::ZERO,
            cooldown_entered_at: None,
            cooldown_depth: 0,
            cooldown_elapsed: Duration::ZERO,
            queue_entered_at: None,
            queue_depth: 0,
            queue_elapsed: Duration::ZERO,
            spawn_started_at: None,
            emitted: false,
        }
    }

    /// Marks entry into provider event handling.
    pub(crate) fn handler_started(&mut self) {
        self.handler_started_at = Some(Instant::now());
    }

    /// Closes event cloning and handler materialization.
    pub(crate) fn handler_materialized(&mut self) {
        self.handler_elapsed = self
            .handler_started_at
            .map_or(Duration::ZERO, |started| started.elapsed());
        self.handler_materialized_at = Some(Instant::now());
    }

    /// Closes handler dispatch before settings resolution starts.
    pub(crate) fn handler_dispatched(&mut self) {
        self.handler_dispatch_elapsed = self
            .handler_materialized_at
            .take()
            .map_or(Duration::ZERO, |started| started.elapsed());
    }

    /// Records the selected settings clone and its bounded profile count.
    pub(crate) fn settings_cloned(&mut self, elapsed: Duration, profile_count: usize) {
        self.settings_elapsed = elapsed;
        self.profile_count = profile_count;
    }

    /// Marks one Secret read without retaining its path, request id, or value.
    pub(crate) fn secret_started(&mut self) {
        self.secret_rpc_count = self.secret_rpc_count.saturating_add(1);
        self.secret_started_at = Some(Instant::now());
    }

    /// Records the size and wait of a closed Secret response.
    pub(crate) fn secret_finished(&mut self, bytes: u64) {
        self.secret_bytes = self.secret_bytes.saturating_add(bytes);
        if let Some(started) = self.secret_started_at.take() {
            self.secret_elapsed = self.secret_elapsed.saturating_add(started.elapsed());
        }
    }

    /// Records content-free quota resolution work.
    pub(crate) fn quota_resolved(&mut self, elapsed: Duration) {
        self.quota_elapsed = self.quota_elapsed.saturating_add(elapsed);
    }

    /// Marks entry into closed OAuth refresh work.
    pub(crate) fn oauth_joined(&mut self) {
        self.oauth_started_at = Some(Instant::now());
        self.oauth_class = OAuthClass::Joined;
    }

    /// Marks entry into newly started closed OAuth refresh work.
    pub(crate) fn oauth_started(&mut self) {
        self.oauth_started_at = Some(Instant::now());
        self.oauth_class = OAuthClass::Started;
    }

    /// Closes OAuth network work before Secret publication starts.
    pub(crate) fn oauth_transport_finished(&mut self) {
        if let Some(started) = self.oauth_started_at.take() {
            self.oauth_elapsed = started.elapsed();
        }
    }

    /// Closes OAuth work after refreshed credentials arrived.
    pub(crate) fn oauth_refreshed(&mut self) {
        self.oauth_transport_finished();
        self.oauth_class = OAuthClass::Refreshed;
    }

    /// Closes OAuth work after a bounded failure.
    pub(crate) fn oauth_failed(&mut self) {
        self.oauth_transport_finished();
        self.oauth_class = OAuthClass::Failed;
    }

    /// Marks entry into cooldown scheduling with an observed queue depth.
    pub(crate) fn cooldown_queued(&mut self, depth: usize) {
        self.cooldown_entered_at = Some(Instant::now());
        self.cooldown_depth = depth;
    }

    /// Closes cooldown queue ownership before the next admission stage.
    pub(crate) fn cooldown_dequeued(&mut self) {
        if let Some(entered) = self.cooldown_entered_at.take() {
            self.cooldown_elapsed = self.cooldown_elapsed.saturating_add(entered.elapsed());
        }
    }

    /// Marks entry into the bounded worker-slot queue.
    pub(crate) fn queued(&mut self, depth: usize) {
        self.slot_dequeued();
        self.queue_entered_at = Some(Instant::now());
        self.queue_depth = depth;
    }

    /// Closes worker-slot queue ownership before another stage.
    pub(crate) fn slot_dequeued(&mut self) {
        if let Some(entered) = self.queue_entered_at.take() {
            self.queue_elapsed = self.queue_elapsed.saturating_add(entered.elapsed());
        }
    }

    /// Marks thread-spawn entry.
    pub(crate) fn spawning(&mut self) {
        self.slot_dequeued();
        self.spawn_started_at = Some(Instant::now());
    }

    /// Emits one fixed-schema scalar-only observation at worker start.
    pub(crate) fn worker_started(mut self) {
        self.emit(ReceiptOutcome::Started);
    }

    /// Emits one fixed-schema scalar-only observation for a closed pre-worker
    /// path.
    pub(crate) fn finished_before_worker(mut self, outcome: ReceiptOutcome) {
        self.emit(outcome);
    }

    /// Emits the fixed event using a bounded call-site-owned outcome.
    fn emit(&mut self, outcome: ReceiptOutcome) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        if let Some(started) = self.secret_started_at.take() {
            self.secret_elapsed = self.secret_elapsed.saturating_add(started.elapsed());
        }
        if self.oauth_started_at.is_some() {
            self.oauth_failed();
        } else if matches!(self.oauth_class, OAuthClass::Started | OAuthClass::Joined) {
            self.oauth_class = OAuthClass::Failed;
        }
        let now = Instant::now();
        let queue_elapsed = self.queue_elapsed.saturating_add(
            self.queue_entered_at
                .map_or(Duration::ZERO, |entered| now.duration_since(entered)),
        );
        let spawn_elapsed = self
            .spawn_started_at
            .map_or(Duration::ZERO, |started| now.duration_since(started));
        let cooldown_elapsed = self.cooldown_elapsed.saturating_add(
            self.cooldown_entered_at
                .map_or(Duration::ZERO, |entered| now.duration_since(entered)),
        );
        let reader_queue_elapsed = self.handler_started_at.map_or(Duration::ZERO, |started| {
            started.duration_since(self.decoded_at)
        });
        let total_us = micros(
            self.decode_elapsed
                .saturating_add(now.duration_since(self.decoded_at)),
        );
        let stage_accounted_us = [
            self.decode_elapsed,
            reader_queue_elapsed,
            self.handler_elapsed,
            self.handler_dispatch_elapsed,
            self.settings_elapsed,
            self.secret_elapsed,
            self.oauth_elapsed,
            self.quota_elapsed,
            cooldown_elapsed,
            queue_elapsed,
            spawn_elapsed,
        ]
        .into_iter()
        .map(micros)
        .fold(0_u64, u64::saturating_add);
        let unattributed_us = total_us.saturating_sub(stage_accounted_us);
        tracing::trace!(
            target: RECEIPT_LOG_TARGET,
            frame_bytes = self.frame_bytes,
            frame_read_decode_us = micros(self.decode_elapsed),
            reader_queue_us = micros(reader_queue_elapsed),
            handler_clone_us = micros(self.handler_elapsed),
            handler_dispatch_us = micros(self.handler_dispatch_elapsed),
            settings_clone_us = micros(self.settings_elapsed),
            profile_count = self.profile_count,
            secret_rpc_count = self.secret_rpc_count,
            secret_bytes = self.secret_bytes,
            secret_wait_us = micros(self.secret_elapsed),
            oauth_class = self.oauth_class.as_str(),
            oauth_us = micros(self.oauth_elapsed),
            quota_us = micros(self.quota_elapsed),
            cooldown_queue_us = micros(cooldown_elapsed),
            cooldown_depth = self.cooldown_depth,
            slot_queue_us = micros(queue_elapsed),
            slot_depth = self.queue_depth,
            spawn_us = micros(spawn_elapsed),
            stage_accounted_us,
            unattributed_us,
            receipt_to_worker_us = total_us,
            outcome = outcome.as_str(),
            "provider receipt observation"
        );
    }
}

impl Drop for ReceiptObservation {
    /// Closes every otherwise-abandoned ownership path exactly once.
    fn drop(&mut self) {
        self.emit(ReceiptOutcome::Failed);
    }
}

/// Saturating scalar projection used by every duration field.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
