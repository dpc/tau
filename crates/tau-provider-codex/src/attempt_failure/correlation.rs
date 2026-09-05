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

    /// Convert to the bounded provider-response attempt ordinal.
    #[must_use]
    pub fn provider_attempt(self) -> tau_proto::ProviderAttempt {
        let value = u32::try_from(self.0).unwrap_or(u32::MAX);
        tau_proto::ProviderAttempt::new(value).expect("logical attempt is nonzero")
    }
}

/// Exact correlation for one upstream dispatch within a logical attempt.
#[derive(Clone, Debug)]
pub(crate) struct DispatchCorrelation {
    /// One-based scheduler attempt owning this dispatch.
    logical_attempt: LogicalAttempt,
    /// One-based upstream dispatch within that attempt.
    wire_dispatch_index: u64,
    /// Private finite attempt; absent for unsupported operations.
    pub(crate) diagnostic: Option<std::sync::Arc<crate::cache_diagnostic::CacheAttempt>>,
    /// Whether the backend spent its transparent repair budget.
    pub(crate) repair_used: bool,
    /// Closed backend-selected repair branch.
    pub(crate) repair_reason: &'static str,
    /// Closed pool-selected connection lifecycle fact.
    pub(crate) connection_state: &'static str,
}

impl DispatchCorrelation {
    /// Preserve unsent exact-request evidence without inventing a wire
    /// dispatch.
    pub(crate) fn undispatched(mut self) -> Self {
        self.wire_dispatch_index = 0;
        self
    }
    /// Return the persisted logical-attempt ordinal.
    #[must_use]
    pub(crate) fn logical_attempt(&self) -> u64 {
        self.logical_attempt.get()
    }

    /// Return the persisted per-attempt wire-dispatch index.
    #[must_use]
    pub(crate) fn wire_dispatch_index(&self) -> u64 {
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
    /// Highest cumulative response-byte count observed across wire dispatches.
    response_bytes_received: u64,
    /// Sticky parser-accepted semantic progress across wire dispatches.
    semantic_progress: crate::SemanticProgress,
    /// Capture-only correlation selected at finite operation entry.
    pub(crate) diagnostic: Option<std::sync::Arc<crate::cache_diagnostic::CacheAttempt>>,
    /// Closed repair branch supplied by the transport owner.
    pub(crate) repair_reason: &'static str,
}

impl AttemptCaptureCorrelation {
    /// Start one logical attempt before any upstream dispatch.
    #[must_use]
    pub(crate) fn new(logical_attempt: LogicalAttempt) -> Self {
        Self {
            logical_attempt,
            last_wire_dispatch_index: 0,
            repair_used: false,
            response_bytes_received: 0,
            semantic_progress: crate::SemanticProgress::None,
            diagnostic: None,
            repair_reason: "none",
        }
    }

    /// Allocate the next one-based wire-dispatch correlation.
    #[must_use]
    pub(crate) fn next_dispatch(&mut self) -> DispatchCorrelation {
        self.last_wire_dispatch_index = self.last_wire_dispatch_index.saturating_add(1);
        DispatchCorrelation {
            logical_attempt: self.logical_attempt,
            wire_dispatch_index: self.last_wire_dispatch_index,
            diagnostic: self.diagnostic.clone(),
            repair_used: self.repair_used,
            repair_reason: self.repair_reason,
            connection_state: "unknown",
        }
    }

    /// Record that this attempt spent its one transparent repair, including an
    /// upgrade failure before another request dispatch.
    pub(crate) fn mark_repair_used(&mut self) {
        self.repair_used = true;
    }

    /// Retain private transport/parser observations without publishing compact
    /// response deltas to extensions.
    pub(crate) fn observe_stream(&mut self, state: &crate::StreamState) {
        self.response_bytes_received = self
            .response_bytes_received
            .max(state.response_bytes_received());
        if state.has_semantic_progress() {
            self.semantic_progress = crate::SemanticProgress::Parsed;
        }
    }

    /// Snapshot attempt facts for one final failure capture.
    #[must_use]
    pub(crate) fn snapshot(&self) -> AttemptCaptureSnapshot {
        AttemptCaptureSnapshot {
            logical_attempt: self.logical_attempt,
            wire_dispatches: self
                .diagnostic
                .as_ref()
                .map_or(self.last_wire_dispatch_index, |d| d.dispatch_count()),
            repair_used: self.repair_used,
            response_bytes_received: self.response_bytes_received,
            semantic_progress: self.semantic_progress,
            attempt_id: self.diagnostic.as_ref().map(|d| d.id),
        }
    }
}

/// Immutable final correlation facts for one failed finite attempt.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AttemptCaptureSnapshot {
    /// Capture-local identity, absent for unsupported operations.
    pub(crate) attempt_id: Option<tau_provider::cache_diagnostic::DiagnosticId>,
    /// One-based scheduler attempt owning the failure.
    logical_attempt: LogicalAttempt,
    /// Count of request envelopes actually dispatched.
    wire_dispatches: u64,
    /// Whether the pool attempted transparent fresh-socket repair.
    repair_used: bool,
    /// Highest cumulative response-byte count observed across dispatches.
    response_bytes_received: u64,
    /// Sticky parser-accepted semantic progress across dispatches.
    semantic_progress: crate::SemanticProgress,
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

    /// Return cumulative response bytes observed across wire dispatches.
    #[must_use]
    pub(crate) fn response_bytes_received(self) -> u64 {
        self.response_bytes_received
    }

    /// Return sticky semantic progress observed across wire dispatches.
    #[must_use]
    pub(crate) fn semantic_progress(self) -> crate::SemanticProgress {
        self.semantic_progress
    }
}
