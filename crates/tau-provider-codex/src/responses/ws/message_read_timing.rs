//! Optional complete-text-message observations scoped to the existing owner.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Connection-local observation gate, independent of transport ownership.
#[derive(Default)]
pub(super) struct MessageReadTiming {
    /// Odd generations enable observations; even generations are inactive.
    generation: AtomicU64,
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
    /// Enable only selected owner scopes; exhaustion leaves timing unavailable.
    #[allow(deprecated, reason = "fetch_update supports the workspace MSRV")]
    pub(super) fn activate(self: &Arc<Self>) -> Option<ActiveMessageReadTiming> {
        let previous = self
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation
                    .is_multiple_of(2)
                    .then(|| generation.checked_add(2).map(|_| generation + 1))
                    .flatten()
            })
            .ok()?;
        Some(ActiveMessageReadTiming {
            timing: Arc::clone(self),
            generation: previous + 1,
        })
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
