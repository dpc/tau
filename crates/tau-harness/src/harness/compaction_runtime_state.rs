//! Owns live standalone-compaction transaction and continuation state.
//!
//! Prompt response snapshots remain with prompt runtime state. This owner
//! retains compaction authority across publication and tool settlement cuts.

use super::*;

/// Runtime-only ownership for manual, reactive, and UI compaction work.
#[derive(Default)]
pub(crate) struct CompactionRuntimeState {
    /// Starts whose post-commit reaction must not dispatch remote work.
    pub(super) suppressed_dispatches:
        HashSet<(tau_proto::AgentId, tau_proto::CompactionTransactionId)>,
    /// Automatic starts that must commit a bounded local failure instead of
    /// dispatching an oversized indivisible prefix to a provider.
    pub(super) preflight_failures: HashMap<
        (tau_proto::AgentId, tau_proto::CompactionTransactionId),
        tau_proto::StandaloneCompactionFailureReason,
    >,
    /// Failures that clean runtime state without provider-watch projection.
    pub(super) silent_failure_prompts: HashSet<AgentPromptId>,
    /// Reactive claims that must terminalize immediately after start commit.
    pub(super) cancelled_claims: HashSet<(tau_proto::AgentId, tau_proto::CompactionTransactionId)>,
    /// Model tool calls awaiting one durable compaction terminal.
    pub(super) pending_manual_tools: HashMap<
        (tau_proto::AgentId, tau_proto::CompactionTransactionId),
        PendingManualCompactionTool,
    >,
    /// Accepted manual requests waiting for a safe start boundary.
    pub(super) accepted_manual_tools:
        HashMap<tau_proto::CompactionRequestId, AcceptedManualCompactionTool>,
    /// Model-tool requests staged until their acceptance fact commits.
    pub(super) pending_model_acceptances:
        HashMap<tau_proto::CompactionRequestId, StagedManualCompactionTool>,
    /// UI compactions waiting for a claimed wait cancellation to commit.
    pub(super) pending_ui_after_wait: HashMap<AgentId, PendingUiCompactionAfterWait>,
    /// Requesting UIs awaiting the durable acceptance commit.
    pub(super) pending_ui_acknowledgements:
        HashMap<tau_proto::CompactionRequestId, Vec<tau_proto::ConnectionId>>,
    /// UI requests staged until their acceptance fact commits.
    pub(super) pending_ui_acceptances:
        HashMap<tau_proto::CompactionRequestId, AcceptedManualCompactionTool>,
    /// Committed UI starts retained through their one transaction terminal.
    pub(super) active_ui_transactions: HashMap<
        (tau_proto::AgentId, tau_proto::CompactionTransactionId),
        tau_proto::CompactionRequestId,
    >,
    /// Exact UI start publications retained after an append rejection.
    pub(super) rejected_ui_starts: HashMap<AgentId, Event>,
    /// Standalone inference checkpoints currently queued through publication.
    pub(super) enqueued_inference_checkpoints:
        HashSet<(tau_proto::AgentId, tau_proto::CompactionTransactionId)>,
}
