//! Session-initialization absolute provider deadline.

use std::time::{Duration, Instant};

/// Maximum total wait while any registered context provider remains
/// outstanding.
const ABSOLUTE_TIMEOUT: Duration = Duration::from_secs(30);

/// Tracks the bounded provider-wait portion of session initialization.
pub(crate) struct SessionInitDeadline {
    /// Non-renewable upper bound for the complete provider wait.
    absolute: Instant,
}

impl SessionInitDeadline {
    /// Starts the session-initialization deadline at `now`.
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            absolute: now + ABSOLUTE_TIMEOUT,
        }
    }

    /// Builds a deterministic explicit deadline for production-waiter tests.
    #[cfg(test)]
    pub(crate) fn for_test(absolute: Instant) -> Self {
        Self { absolute }
    }

    /// Returns the deadline at which provider waiting must stop.
    pub(crate) fn next_deadline(&self) -> Instant {
        self.absolute
    }

    /// Reports whether the absolute provider deadline has been reached.
    pub(crate) fn expired(&self, now: Instant) -> bool {
        self.absolute <= now
    }
}

#[cfg(test)]
mod tests;
