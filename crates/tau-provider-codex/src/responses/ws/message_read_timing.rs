//! Optional complete-text-message observations scoped to the existing owner.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Connection-local observation gate, independent of transport ownership.
#[derive(Default)]
pub(super) struct MessageReadTiming {
    /// Odd generations enable observations; even generations are inactive.
    generation: AtomicU64,
    /// Constant-size received control-frame evidence for the active owner.
    controls: Mutex<ControlObservations>,
}

/// Aggregate receive facts, not sent pings or matched heartbeat exchanges.
#[derive(Clone, Copy, Default)]
pub(super) struct ControlObservations {
    /// Received Ping count, saturating instead of wrapping.
    pub(super) ping_count: u64,
    /// Received Pong count, without payload matching.
    pub(super) pong_count: u64,
    /// Last received Ping availability before any channel admission.
    pub(super) last_ping_at: Option<Instant>,
    /// Last received Pong availability before any channel admission.
    pub(super) last_pong_at: Option<Instant>,
}

/// One reader observation, carried only alongside its existing text message.
pub(super) struct MessageRead {
    /// Owner generation that was active at the reader boundary.
    generation: u64,
    /// Complete Tungstenite text message availability, before channel
    /// admission.
    read_at: Instant,
}

/// Deactivates observations on every exit from the existing envelope owner.
pub(super) struct ActiveMessageReadTiming {
    /// Connection-local gate shared with the existing reader.
    timing: Arc<MessageReadTiming>,
    /// Unique active owner generation; never reused on this connection.
    generation: u64,
}

impl MessageReadTiming {
    /// Enable one envelope owner's bounded observations; exhaustion disables
    /// them.
    #[allow(deprecated, reason = "fetch_update supports the workspace MSRV")]
    pub(super) fn activate(self: &Arc<Self>) -> Option<ActiveMessageReadTiming> {
        let mut controls = self.controls.lock().expect("control observation lock");
        let previous = self
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation
                    .is_multiple_of(2)
                    .then(|| generation.checked_add(2).map(|_| generation + 1))
                    .flatten()
            })
            .ok()?;
        *controls = ControlObservations::default();
        Some(ActiveMessageReadTiming {
            timing: Arc::clone(self),
            generation: previous + 1,
        })
    }

    /// Observe a control frame without queueing it, waking the owner, or
    /// granting application-message liveness. No payload is inspected.
    pub(super) fn observe_control(&self, ping: bool) {
        let generation = self.generation.load(Ordering::Acquire);
        if generation.is_multiple_of(2) {
            return;
        }
        let mut controls = self.controls.lock().expect("control observation lock");
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        let now = Instant::now();
        if ping {
            controls.ping_count = controls.ping_count.saturating_add(1);
            controls.last_ping_at = Some(now);
        } else {
            controls.pong_count = controls.pong_count.saturating_add(1);
            controls.last_pong_at = Some(now);
        }
    }

    /// Observe a complete text message only inside an active owner generation.
    ///
    /// No clock is read while inactive. A concurrent owner exit invalidates the
    /// sample; a queued observation cannot be attributed to the next owner.
    pub(super) fn observe(&self) -> Option<MessageRead> {
        let generation = self.generation.load(Ordering::Acquire);
        if generation.is_multiple_of(2) {
            return None;
        }
        let read_at = Instant::now();
        (self.generation.load(Ordering::Acquire) == generation).then_some(MessageRead {
            generation,
            read_at,
        })
    }
}

impl ActiveMessageReadTiming {
    /// Snapshot the active envelope's bounded received control-frame evidence.
    pub(super) fn controls(&self) -> ControlObservations {
        *self
            .timing
            .controls
            .lock()
            .expect("control observation lock")
    }
    /// Accept only messages sampled within this owner, not queued prior turns.
    pub(super) fn read_at(&self, observation: Option<MessageRead>) -> Option<Instant> {
        observation
            .filter(|observation| observation.generation == self.generation)
            .map(|observation| observation.read_at)
    }
}

impl Drop for ActiveMessageReadTiming {
    fn drop(&mut self) {
        self.timing
            .generation
            .store(self.generation + 1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
