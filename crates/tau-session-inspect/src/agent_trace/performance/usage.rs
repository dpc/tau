//! Response-local token evidence.

/// Content-free response-local token counters.
pub(super) struct Usage {
    /// Sent input tokens.
    sent: u64,
    /// Cached input tokens capped at sent.
    cached: u64,
    /// Received output tokens.
    received: u64,
}

impl Usage {
    /// Builds counters while enforcing the accounting cache bound.
    pub(super) fn new(sent: u64, cached: u64, received: u64) -> Self {
        Self {
            sent,
            cached: cached.min(sent),
            received,
        }
    }

    /// Returns sent input tokens.
    pub(super) fn sent(&self) -> u64 {
        self.sent
    }
    /// Returns capped cached input tokens.
    pub(super) fn cached(&self) -> u64 {
        self.cached
    }
    /// Returns received output tokens.
    pub(super) fn received(&self) -> u64 {
        self.received
    }
}
