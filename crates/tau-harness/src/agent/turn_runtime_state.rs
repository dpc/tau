//! Active outer-turn state and terminal-processing obligations.

use std::collections::BTreeMap;

use super::{AgentTurnState, OuterTurnRuntimeState, OutputLengthContinuationState, WorkStatus};

/// Exclusive runtime phase of one eager automatic-compaction transaction.
#[derive(Debug, Default)]
pub(crate) enum AutomaticCompactionRuntimeState {
    /// No eager decision or protected start is pending.
    #[default]
    None,
    /// A committed terminal decision is owed by its outer-turn finish.
    DecisionOwed(tau_proto::CompactionTransactionId),
    /// The decision's protected start awaits interception or persistence.
    StartPending(tau_proto::CompactionTransactionId),
}

impl AutomaticCompactionRuntimeState {
    /// Return the transaction correlation owned by either pending phase.
    pub(crate) fn transaction_id(&self) -> Option<&tau_proto::CompactionTransactionId> {
        match self {
            Self::None => None,
            Self::DecisionOwed(transaction_id) | Self::StartPending(transaction_id) => {
                Some(transaction_id)
            }
        }
    }

    /// Return the decision correlation only while the outer-turn finish owes
    /// it.
    pub(crate) fn decision_id(&self) -> Option<&tau_proto::CompactionTransactionId> {
        match self {
            Self::DecisionOwed(transaction_id) => Some(transaction_id),
            Self::None | Self::StartPending(_) => None,
        }
    }

    /// Return whether the protected start owns `transaction_id`.
    pub(crate) fn start_is_pending_for(
        &self,
        transaction_id: &tau_proto::CompactionTransactionId,
    ) -> bool {
        matches!(self, Self::StartPending(pending) if pending == transaction_id)
    }

    /// Record a committed eager decision awaiting its outer-turn finish.
    pub(crate) fn record_decision(&mut self, transaction_id: tau_proto::CompactionTransactionId) {
        *self = Self::DecisionOwed(transaction_id);
    }

    /// Record the protected start while it awaits interception or persistence.
    pub(crate) fn record_start(&mut self, transaction_id: tau_proto::CompactionTransactionId) {
        *self = Self::StartPending(transaction_id);
    }

    /// Clear only the decision phase after its outer-turn finish commits.
    pub(crate) fn clear_decision(&mut self) {
        if matches!(self, Self::DecisionOwed(_)) {
            *self = Self::None;
        }
    }

    /// Clear the protected start only when its exact correlation settles.
    pub(crate) fn clear_start(&mut self, transaction_id: &tau_proto::CompactionTransactionId) {
        if self.start_is_pending_for(transaction_id) {
            *self = Self::None;
        }
    }
}

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
    pub(crate) turn_generation: tau_proto::AgentOuterTurnGeneration,
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
    /// Exclusive eager decision/start phase for automatic compaction.
    pub(crate) automatic_compaction: AutomaticCompactionRuntimeState,
    /// Typed runtime ownership of the open turn and its write-pending finish.
    pub(crate) outer_turn: OuterTurnRuntimeState,
    /// Named runtime state for the current reasoning-only run's continuation.
    pub(crate) output_length_continuation: OutputLengthContinuationState,
    /// Whether the current turn was caused only by lifecycle notifications.
    pub(crate) lifecycle_notification_only_turn: bool,
}

#[cfg(test)]
mod tests;
