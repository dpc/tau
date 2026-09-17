//! Read-only output-length enrichment and its deferred runtime changes.

use super::*;

/// Exact output-length fields and runtime changes prepared for one terminal.
///
/// Preparation does not reserve an identity or install continuation ownership.
/// The caller applies this immediately at the existing derivation boundary.
#[derive(Default)]
pub(super) struct PreparedOutputLengthContinuation {
    /// Canonical disposition to attach even when no continuation is eligible.
    pub(super) disposition: tau_proto::OutputLengthDisposition,
    /// Next identity cursor, including the historical missing-checkpoint
    /// advance.
    pub(super) next_prompt_index: Option<u64>,
    /// New ownership to install only when the source checkpoint exists.
    pub(super) plan: Option<path_crate_agent::OutputLengthContinuationPlan>,
}
