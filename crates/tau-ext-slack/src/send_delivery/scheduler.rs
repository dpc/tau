//! Event-driven production and deterministic test scheduling for Slack sends.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Event-driven cancellation generation shared by delivery workers.
#[derive(Default)]
pub(crate) struct SendWake {
    /// Monotonic lifecycle generation.
    generation: Mutex<u64>,
    /// Wakes retry waits after lifecycle changes.
    changed: Condvar,
}

impl SendWake {
    /// Capture the current cancellation generation.
    pub(crate) fn generation(&self) -> u64 {
        *self
            .generation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Cancel current waits and advance the generation for future workers.
    pub(crate) fn notify_lifecycle_change(&self) {
        let mut generation = self
            .generation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *generation = generation.wrapping_add(1);
        self.changed.notify_all();
    }

    /// Wake waiters after channel-queue progress without implying revocation.
    pub(crate) fn notify_progress(&self) {
        self.notify_lifecycle_change();
    }

    /// Wait until the delay expires or the captured generation is cancelled.
    ///
    /// Returns `true` when cancellation won.
    pub(crate) fn wait(&self, observed: u64, delay: Duration) -> bool {
        let generation = self
            .generation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *generation != observed {
            return true;
        }
        let (generation, _) = self
            .changed
            .wait_timeout_while(generation, delay, |current| *current == observed)
            .unwrap_or_else(|error| error.into_inner());
        *generation != observed
    }
}

/// Injectable event-driven retry scheduler.
pub(crate) trait SendScheduler: Send + Sync + 'static {
    /// Wait for the delay or return `true` when lifecycle cancellation wins.
    fn wait(&self, wake: &SendWake, generation: u64, delay: Duration) -> bool;
}

/// Production scheduler backed by a condition variable.
pub(crate) struct SystemSendScheduler;

impl SendScheduler for SystemSendScheduler {
    fn wait(&self, wake: &SendWake, generation: u64, delay: Duration) -> bool {
        wake.wait(generation, delay)
    }
}

/// Test scheduler that advances logical waits immediately.
#[cfg(test)]
pub(crate) struct ImmediateSendScheduler;

#[cfg(test)]
impl SendScheduler for ImmediateSendScheduler {
    fn wait(&self, _wake: &SendWake, _generation: u64, _delay: Duration) -> bool {
        false
    }
}
