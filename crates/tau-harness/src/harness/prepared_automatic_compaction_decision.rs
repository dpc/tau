//! Read-only eager-compaction enrichment and deferred diagnostics.

/// Prepared eager-compaction decision without identity allocation or logging.
#[derive(Default)]
pub(super) struct PreparedAutomaticCompactionDecision {
    /// Canonical authority, absent when any existing eligibility check rejects
    /// it.
    pub(super) decision: Option<tau_proto::AutomaticCompactionDecision>,
    /// Existing coalescing diagnostic, including matches rejected by later
    /// checks.
    pub(super) diagnostic: Option<MatchedCompactionPolicies>,
}

/// Content-free values for the existing policy-coalescing diagnostic.
pub(super) struct MatchedCompactionPolicies {
    /// Comma-separated matching names in policy iteration order.
    pub(super) names: String,
    /// Lowest matching threshold.
    pub(super) threshold: tau_proto::TokenCount,
}
