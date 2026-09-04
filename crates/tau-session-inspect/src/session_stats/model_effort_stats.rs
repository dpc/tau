use serde::Serialize;
use tau_proto::{ModelId, NativeReasoningEffort};

use super::ActivityCounts;

/// One captured model-and-effort accounting bucket.
#[derive(Clone, Debug, Serialize)]
pub struct ModelNativeReasoningEffortStats {
    /// Provider-qualified model identity.
    pub model: ModelId,
    /// Captured reasoning effort.
    pub effort: NativeReasoningEffort,
    /// Exact activity attributed to this dispatch snapshot.
    pub totals: ActivityCounts,
}
