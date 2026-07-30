/// One-based finite-attempt ordinal for one agent prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogicalAttempt(
    /// Persisted one-based scheduler attempt.
    u64,
);

impl LogicalAttempt {
    /// Construct an ordinal, coercing zero to the first attempt.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value.max(1))
    }

    /// Return the persisted one-based ordinal.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Exact correlation for one upstream dispatch within a logical attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DispatchCorrelation {
    /// One-based scheduler attempt owning this dispatch.
    logical_attempt: LogicalAttempt,
    /// One-based upstream dispatch within that attempt.
    wire_dispatch_index: u64,
}

impl DispatchCorrelation {
    /// Return the persisted logical-attempt ordinal.
    #[must_use]
    pub(crate) fn logical_attempt(self) -> u64 {
        self.logical_attempt.get()
    }

    /// Return the persisted per-attempt wire-dispatch index.
    #[must_use]
    pub(crate) fn wire_dispatch_index(self) -> u64 {
        self.wire_dispatch_index
    }
}

/// Attempt-owned dispatch counter. A transparent repair increments only the
/// wire index; a scheduler retry creates a new value starting at one.
#[derive(Debug)]
pub(crate) struct AttemptCaptureCorrelation {
    /// One-based scheduler attempt owning the counter.
    logical_attempt: LogicalAttempt,
    /// Count of request envelopes actually dispatched.
    last_wire_dispatch_index: u64,
    /// Whether the pool attempted transparent fresh-socket repair.
    repair_used: bool,
}

impl AttemptCaptureCorrelation {
    /// Start one logical attempt before any upstream dispatch.
    #[must_use]
    pub(crate) fn new(logical_attempt: LogicalAttempt) -> Self {
        Self {
            logical_attempt,
            last_wire_dispatch_index: 0,
            repair_used: false,
        }
    }

    /// Allocate the next one-based wire-dispatch correlation.
    #[must_use]
    pub(crate) fn next_dispatch(&mut self) -> DispatchCorrelation {
        self.last_wire_dispatch_index = self.last_wire_dispatch_index.saturating_add(1);
        DispatchCorrelation {
            logical_attempt: self.logical_attempt,
            wire_dispatch_index: self.last_wire_dispatch_index,
        }
    }

    /// Record that this attempt spent its one transparent repair, including an
    /// upgrade failure before another request dispatch.
    pub(crate) fn mark_repair_used(&mut self) {
        self.repair_used = true;
    }

    /// Snapshot attempt facts for one final failure capture.
    #[must_use]
    pub(crate) fn snapshot(&self) -> AttemptCaptureSnapshot {
        AttemptCaptureSnapshot {
            logical_attempt: self.logical_attempt,
            wire_dispatches: self.last_wire_dispatch_index,
            repair_used: self.repair_used,
        }
    }
}

/// Immutable final correlation facts for one failed finite attempt.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AttemptCaptureSnapshot {
    /// One-based scheduler attempt owning the failure.
    logical_attempt: LogicalAttempt,
    /// Count of request envelopes actually dispatched.
    wire_dispatches: u64,
    /// Whether the pool attempted transparent fresh-socket repair.
    repair_used: bool,
}

impl AttemptCaptureSnapshot {
    /// Return the one-based logical attempt.
    #[must_use]
    pub(crate) fn logical_attempt(self) -> u64 {
        self.logical_attempt.get()
    }

    /// Return the number of request envelopes actually dispatched.
    #[must_use]
    pub(crate) fn wire_dispatches(self) -> u64 {
        self.wire_dispatches
    }

    /// Return whether a fresh-socket repair was attempted.
    #[must_use]
    pub(crate) fn repair_used(self) -> bool {
        self.repair_used
    }
}
