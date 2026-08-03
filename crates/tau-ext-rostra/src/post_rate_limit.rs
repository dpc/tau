//! Runtime-only rolling quota for self-authored Rostra social-post attempts.

use std::collections::VecDeque;
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::{Duration, Instant};

/// Configures the shared rolling quota for posts, replies, and reactions.
#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct PostRateLimit {
    /// Number of post-like admissions allowed in one rolling window.
    max_events: NonZeroUsize,
    /// Width of the rolling window in seconds.
    window_seconds: NonZeroU64,
}

impl Default for PostRateLimit {
    fn default() -> Self {
        Self {
            max_events: NonZeroUsize::new(10).expect("default maximum is nonzero"),
            window_seconds: NonZeroU64::new(3_600).expect("default window is nonzero"),
        }
    }
}

impl PostRateLimit {
    /// Return the configured rolling-window duration.
    fn window(self) -> Duration {
        Duration::from_secs(self.window_seconds.get())
    }
}

/// Runtime-only admissions retained for one configured Rostra identity.
#[derive(Debug, Default)]
pub(crate) struct PostRateLimitWindow {
    /// Monotonic times at which this process admitted post-like writes.
    admitted_at: VecDeque<Instant>,
}

/// A full runtime quota's bounded retry metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RateLimitExceeded {
    /// Whole seconds until the quota-filling threshold leaves the window.
    pub(crate) retry_after_seconds: u64,
}

impl PostRateLimitWindow {
    /// Reserve one post-like attempt or return its retry metadata.
    ///
    /// Callers hold the extension's serialized write lane. This reservation
    /// intentionally happens before signing and is never rolled back.
    pub(crate) fn reserve(&mut self, limit: PostRateLimit) -> Result<(), RateLimitExceeded> {
        self.reserve_at(limit, Instant::now())
    }

    /// Prune expired admissions and make a deterministic reservation decision.
    fn reserve_at(&mut self, limit: PostRateLimit, now: Instant) -> Result<(), RateLimitExceeded> {
        let window = limit.window();
        while self
            .admitted_at
            .front()
            .is_some_and(|timestamp| window <= now.saturating_duration_since(*timestamp))
        {
            self.admitted_at.pop_front();
        }

        if limit.max_events.get() <= self.admitted_at.len() {
            let threshold = self
                .admitted_at
                .get(self.admitted_at.len() - limit.max_events.get())
                .expect("a full quota has a threshold timestamp");
            let remaining = window.saturating_sub(now.saturating_duration_since(*threshold));
            return Err(RateLimitExceeded {
                retry_after_seconds: ceil_seconds(remaining),
            });
        }

        self.admitted_at.push_back(now);
        Ok(())
    }
}

/// Round a nonzero duration upward so retrying after the reported integer
/// works.
fn ceil_seconds(duration: Duration) -> u64 {
    duration.as_secs().saturating_add(u64::from(
        !duration.is_zero() && duration.subsec_nanos() != 0,
    ))
}

#[cfg(test)]
mod tests;
