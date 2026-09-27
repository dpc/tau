//! Process-local inactivity clock for opt-in session shutdown.

use std::time::{Duration, Instant};

/// Tracks the most recent meaningful activity without persisting an idle clock.
pub(crate) struct IdleSession {
    /// Disabled unless a positive duration was configured at startup.
    limit: Option<Duration>,
    /// Last observed work or accepted user/session activity.
    last_activity: Instant,
    /// Whether the previous runtime checkpoint had accepted work.
    was_busy: bool,
}

impl IdleSession {
    /// Start a fresh monotonic window on each daemon launch, including resume.
    pub(super) fn new(limit: Option<Duration>) -> Self {
        Self {
            limit,
            last_activity: Instant::now(),
            was_busy: false,
        }
    }

    /// Reset the idle window after work, or after an accepted active input.
    pub(super) fn checkpoint(&mut self, now: Instant, busy: bool, activity: bool) {
        if busy || self.was_busy || activity {
            self.last_activity = now;
        }
        self.was_busy = busy;
    }

    /// Return the wake-up time only when no accepted work remains.
    pub(super) fn deadline(&self) -> Option<Instant> {
        (!self.was_busy)
            .then_some(self.limit)
            .flatten()
            .and_then(|limit| self.last_activity.checked_add(limit))
    }
}
