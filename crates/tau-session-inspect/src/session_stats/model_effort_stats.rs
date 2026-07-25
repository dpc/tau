use serde::Serialize;
use tau_proto::{Effort, ModelId};

use super::ActivityCounts;

/// One captured model-and-effort accounting bucket.
#[derive(Clone, Debug, Serialize)]
pub struct ModelEffortStats {
    /// Provider-qualified model identity.
    pub model: ModelId,
    /// Captured reasoning effort.
    pub effort: Effort,
    /// Exact activity attributed to this dispatch snapshot.
    pub totals: ActivityCounts,
}
