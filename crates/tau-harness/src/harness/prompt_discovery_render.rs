//! Content-only inputs retained by an undelivered prompt's unique continuation.

use super::*;

/// Captured tool surface and bootstrap ownership needed for a discovery
/// refresh.
#[derive(Clone)]
pub(crate) struct PromptDiscoveryRender {
    /// Discovery revision represented by the current request bytes.
    pub(crate) revision: u64,
    /// Original role, without selecting a new dispatch role on refresh.
    pub(crate) role: String,
    /// Exact prompt capability surface, including hidden/hosted distinctions.
    pub(crate) capabilities: Vec<tau_proto::ToolSpec>,
    /// Original sorted provider-fragment inputs, not a new tool registration
    /// scan.
    pub(crate) providers: Vec<tau_core::ToolProvider>,
    /// Original effective capability names used for fragment selection.
    pub(crate) effective_names: HashSet<ToolName>,
    /// Whether the synthetic initialization block occupies index zero.
    pub(crate) has_bootstrap: bool,
    /// Provenance guidance required by history independently of the bootstrap.
    pub(crate) history_provenance: bool,
}

/// A live checkpoint callback paused before materialization, never replay
/// repair.
pub(crate) struct DiscoveryCheckpointResume {
    /// Loaded runtime route that owns this callback.
    pub(crate) cid: AgentId,
    /// Exact prompt identity committed by the original checkpoint.
    pub(crate) prompt_id: AgentPromptId,
    /// Runtime incarnation that must still own this checkpoint.
    pub(crate) runtime_incarnation: u64,
    /// Original content-free timing owner, without starting another request.
    pub(crate) timing: Option<super::prompt_materialization_timing::PromptMaterializationTiming>,
}
