//! Read-only standalone failure shaping before terminal effects.

/// Exact failed-compaction fact and the local metadata needed when applying it.
pub(super) struct PreparedStandaloneFailure {
    /// Durable fact, including any pre-minted retreat or incomplete output.
    pub(super) failed: tau_proto::AgentStandaloneCompactionFailed,
    /// Explicit selected parent at the preparation cut.
    pub(super) batch_parent: tau_proto::AgentHead,
    /// Original diagnostic reason before irreducibility refinement.
    pub(super) diagnostic_reason: tau_proto::StandaloneCompactionFailureReason,
}
