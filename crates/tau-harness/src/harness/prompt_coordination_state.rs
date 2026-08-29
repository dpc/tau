//! Prompt construction, dispatch, cancellation, and compaction ownership.

use super::*;

/// State coordinating the complete prompt lifecycle.
pub(crate) struct PromptCoordinationState {
    /// Prompt correlation, dispatch snapshots, and replay state.
    pub(crate) prompt_runtime: PromptRuntimeState,
    /// Standalone compaction runtime state.
    pub(crate) compaction_runtime: CompactionRuntimeState,
    /// Canonical standalone backend-attempt accounting ownership.
    pub(crate) standalone_accounting: StandaloneExecutionAccountingState,
    /// Skill, context-provider, preview, and template discovery state.
    pub(crate) context_discovery: ContextDiscoveryState,
    /// Notices waiting to be folded into a real user prompt.
    pub(crate) pending_notices: PendingPromptNoticeState,
    /// Prompt identifiers canceled by the user.
    pub(crate) canceled_prompts: HashSet<AgentPromptId>,
}
