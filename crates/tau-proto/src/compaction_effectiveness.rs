//! Observational, same-snapshot compaction estimates.

use serde::{Deserialize, Serialize};

use crate::TokenCount;

/// Estimated ordinary-request sizes at one accepted compaction boundary.
///
/// All four values use one estimator and one frozen system/tools recipe. The
/// prefix includes any previous replacement; both totals include the same
/// retained suffix. These are not provider usage or scheduling authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactionEffectiveness {
    /// Versioned estimator identity; persisted values are never recomputed on
    /// replay.
    pub estimator: String,
    /// Estimated contribution of the history actually replaced.
    pub prefix_before: Option<TokenCount>,
    /// Estimated whole ordinary request before replacement.
    pub total_before: Option<TokenCount>,
    /// Estimated contribution of the installed replacement, in its retained
    /// role.
    pub replacement_after: Option<TokenCount>,
    /// Estimated whole ordinary request after replacement.
    pub total_after: Option<TokenCount>,
}
