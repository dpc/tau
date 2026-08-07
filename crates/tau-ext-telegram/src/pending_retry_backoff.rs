//! Bounded retry timing for unconfirmed local Telegram updates.

use std::time::Duration;

/// Initial retry delay after Telegram redelivers an unconfirmed routed update.
const INITIAL_PENDING_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Maximum retry delay while a canonical echo remains missing.
const MAX_PENDING_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Bounded exponential retry schedule for pending Telegram redelivery.
pub(super) struct PendingRetryBackoff {
    /// Delay returned by the next retry.
    next_delay: Duration,
}

impl PendingRetryBackoff {
    /// Start a fresh bounded retry schedule.
    pub(super) fn new() -> Self {
        Self {
            next_delay: INITIAL_PENDING_RETRY_DELAY,
        }
    }

    /// Reset after cursor progress or after the pending queue empties.
    pub(super) fn reset(&mut self) {
        self.next_delay = INITIAL_PENDING_RETRY_DELAY;
    }

    /// Return this retry's delay and exponentially bound the following retry.
    pub(super) fn take_delay(&mut self) -> Duration {
        let delay = self.next_delay;
        self.next_delay = self
            .next_delay
            .saturating_mul(2)
            .min(MAX_PENDING_RETRY_DELAY);
        delay
    }
}
