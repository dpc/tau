//! Session-initialization idle and absolute provider deadlines.

use std::time::{Duration, Instant};

/// Maximum silence between accepted contributions from outstanding providers.
const IDLE_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum total wait while any registered context provider remains
/// outstanding.
const ABSOLUTE_TIMEOUT: Duration = Duration::from_secs(30);

/// Monotonic marker for accepted progress by an outstanding session provider.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SessionInitProgressGeneration(
    /// Number of accepted contributions observed from outstanding providers.
    u64,
);

impl SessionInitProgressGeneration {
    /// Advances the marker after one accepted discovery or readiness event.
    pub(crate) fn advance(&mut self) {
        self.0 = self.0.saturating_add(1);
    }
}

/// Tracks the bounded provider-wait portion of session initialization.
pub(crate) struct SessionInitDeadline {
    /// Deadline renewed by accepted progress from an outstanding provider.
    idle: Instant,
    /// Non-renewable upper bound for the complete provider wait.
    absolute: Instant,
    /// Last harness progress generation reflected in [`Self::idle`].
    progress_generation: SessionInitProgressGeneration,
}

impl SessionInitDeadline {
    /// Starts both session-initialization deadlines at `now`.
    pub(crate) fn new(now: Instant, progress_generation: SessionInitProgressGeneration) -> Self {
        Self {
            idle: now + IDLE_TIMEOUT,
            absolute: now + ABSOLUTE_TIMEOUT,
            progress_generation,
        }
    }

    /// Builds deterministic explicit deadlines for production-waiter tests.
    #[cfg(test)]
    pub(crate) fn for_test(
        idle: Instant,
        absolute: Instant,
        progress_generation: SessionInitProgressGeneration,
    ) -> Self {
        Self {
            idle,
            absolute,
            progress_generation,
        }
    }

    /// Returns the next deadline at which provider waiting must stop.
    pub(crate) fn next_deadline(&self) -> Instant {
        self.idle.min(self.absolute)
    }

    /// Renews the idle deadline when accepted provider progress advanced.
    ///
    /// The caller supplies the time at which it received the contributing
    /// event, so synchronous harness processing does not move
    /// provider-owned deadlines.
    pub(crate) fn observe_progress(
        &mut self,
        received_at: Instant,
        progress_generation: SessionInitProgressGeneration,
    ) {
        if self.progress_generation == progress_generation {
            return;
        }
        self.progress_generation = progress_generation;
        self.idle = (received_at + IDLE_TIMEOUT).min(self.absolute);
    }

    /// Reports whether the next provider deadline has been reached.
    pub(crate) fn expired(&self, now: Instant) -> bool {
        self.next_deadline() <= now
    }
}

#[cfg(test)]
mod tests;
