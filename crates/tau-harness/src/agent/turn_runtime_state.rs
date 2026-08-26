//! Active outer-turn state and terminal-processing obligations.

use std::collections::BTreeMap;

use super::{AgentTurnState, OuterTurnRuntimeState, OutputLengthContinuationState, WorkStatus};

/// Runtime state owned by the active outer turn and its terminal processing.
#[derive(Debug)]
pub(crate) struct AgentTurnRuntimeState {
    /// Internal provider/tool phase for the active turn.
    pub(crate) turn_state: AgentTurnState,
    /// Last externally published runtime state, independent of internal
    /// continuation bookkeeping that may temporarily use `Idle`.
    pub(crate) published_runtime_state: tau_proto::AgentRuntimeState,
    /// Runtime-scoped outer agent-turn generation used by provider-status
    /// notifications.
    pub(crate) turn_generation: u64,
    /// Runtime-only semantic progress reported through the status tool.
    pub(crate) work_status: WorkStatus,
    /// Whether the final canonical prompt of the current outer turn exposed
    /// status.
    pub(crate) terminal_status_was_available: bool,
    /// Whether the accepted terminal may create an outer-finish notice.
    pub(crate) terminal_notice_eligible: bool,
    /// Exact outer turn owning the current runtime-only notice candidate.
    pub(crate) terminal_notice_outer_turn_id: Option<tau_proto::AgentOuterTurnId>,
    /// Alert policy frozen with the final accepted prompt of that turn.
    pub(crate) terminal_context_size_alerts:
        BTreeMap<String, tau_config::settings::ContextSizeAlert>,
    /// Durable eager decision committed by the current turn's terminal and owed
    /// by its outer-turn finish.
    pub(crate) pending_automatic_compaction_decision: Option<tau_proto::CompactionTransactionId>,
    /// Protected eager start currently waiting on interception or persistence.
    pub(crate) pending_automatic_compaction_start: Option<tau_proto::CompactionTransactionId>,
    /// Typed runtime ownership of the open turn and its write-pending finish.
    pub(crate) outer_turn: OuterTurnRuntimeState,
    /// Named runtime state for the current reasoning-only run's continuation.
    pub(crate) output_length_continuation: OutputLengthContinuationState,
    /// Whether the current turn was caused only by lifecycle notifications.
    pub(crate) lifecycle_notification_only_turn: bool,
}
