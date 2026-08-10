//! Deadlines shared by public Responses SSE and WebSocket streams.

use std::time::{Duration, Instant};

/// Maximum time for one request, connection, or response-header operation.
pub(super) const REQUEST_CONNECT_HEADER_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Maximum silence between qualifying semantic stream observations.
pub(super) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Maximum lifetime of one response body stream after its headers arrive.
pub(super) const STREAM_TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Immutable total and renewable semantic-idle deadlines for one response
/// stream.
///
/// Only an accepted change to qualifying semantic output may renew the idle
/// deadline. Transport chunks, SSE comments, WebSocket control frames, and
/// semantic no-ops leave it unchanged.
#[derive(Clone, Copy, Debug)]
pub(super) struct StreamDeadlines {
    /// Fixed deadline from response-stream start.
    absolute: Instant,
    /// Deadline renewed by qualifying semantic output only.
    semantic_idle: Instant,
}

impl StreamDeadlines {
    /// Starts stream accounting when a successful response body becomes
    /// available.
    pub(super) fn new(now: Instant) -> Self {
        Self {
            absolute: now + STREAM_TOTAL_TIMEOUT,
            semantic_idle: now + STREAM_IDLE_TIMEOUT,
        }
    }

    /// Returns the earlier of the total and semantic-idle deadlines.
    pub(super) fn next_deadline(self) -> Instant {
        self.absolute.min(self.semantic_idle)
    }

    /// Returns whether either deadline has expired at `now`.
    pub(super) fn expired(self, now: Instant) -> bool {
        self.next_deadline() <= now
    }

    /// Renews only semantic-idle time after accepted qualifying stream
    /// progress.
    pub(super) fn renew_for_qualifying_progress(&mut self, now: Instant) {
        self.semantic_idle = now + STREAM_IDLE_TIMEOUT;
    }
}
