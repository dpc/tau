//! Owns live standalone-compaction transaction and continuation state.
//!
//! Prompt response snapshots remain with prompt runtime state. This owner
//! retains compaction authority across publication and tool settlement cuts.

use super::*;

/// Agent-scoped identity of one standalone-compaction transaction.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct CompactionTransaction {
    /// Agent journal that owns the transaction-local identifier.
    agent_id: tau_proto::AgentId,
    /// Correlation identifier unique within the owning agent journal.
    transaction_id: tau_proto::CompactionTransactionId,
}

impl CompactionTransaction {
    /// Construct an agent-scoped transaction identity.
    pub(super) fn new(
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
    ) -> Self {
        Self {
            agent_id,
            transaction_id,
        }
    }

    /// Return whether this transaction belongs to `agent_id`.
    pub(super) fn belongs_to(&self, agent_id: &tau_proto::AgentId) -> bool {
        &self.agent_id == agent_id
    }

    /// Return the transaction-local correlation identifier.
    pub(super) fn transaction_id(&self) -> &tau_proto::CompactionTransactionId {
        &self.transaction_id
    }
}

/// Runtime reason that a committed compaction start must not dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SuppressedStart {
    /// Local prefix validation selected a bounded failure terminal.
    PreflightFailure(tau_proto::StandaloneCompactionFailureReason),
    /// Control-plane cancellation won before the start committed.
    Cancelled,
    /// A correlated terminal publication was already queued with the start.
    TerminalAlreadyQueued,
}

/// Runtime delivery owner for one durably started manual compaction.
#[derive(Clone)]
pub(super) enum ManualCompactionStartOwner {
    /// A UI request needs only coalescing until the transaction terminal.
    Ui {
        /// Durable request correlated with the started transaction.
        #[allow(
            dead_code,
            reason = "the typed owner preserves correlation even though UI terminals need only the transaction"
        )]
        request_id: tau_proto::CompactionRequestId,
    },
    /// A model tool awaits one correlated background completion.
    ModelTool(PendingManualCompactionTool),
}

/// Runtime-only ownership for manual, reactive, and UI compaction work.
#[derive(Default)]
pub(crate) struct CompactionRuntimeState {
    /// Exclusive runtime reasons that committed starts must not dispatch.
    suppressed_starts: HashMap<CompactionTransaction, SuppressedStart>,
    /// Failures that clean runtime state without provider-watch projection.
    pub(super) silent_failure_prompts: HashSet<AgentPromptId>,
    /// Exclusive delivery owner for each durably started manual transaction.
    pub(super) active_manual_transactions:
        HashMap<CompactionTransaction, ManualCompactionStartOwner>,
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
    /// Exact UI start publications retained after an append rejection.
    pub(super) rejected_ui_starts: HashMap<AgentId, Event>,
    /// Standalone inference checkpoints currently queued through publication.
    pub(super) enqueued_inference_checkpoints:
        HashSet<(tau_proto::AgentId, tau_proto::CompactionTransactionId)>,
}

impl CompactionRuntimeState {
    /// Record a local preflight failure as the highest-priority
    /// suppressed-start reaction.
    pub(super) fn suppress_start_for_preflight(
        &mut self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
        reason: tau_proto::StandaloneCompactionFailureReason,
    ) {
        self.suppressed_starts.insert(
            CompactionTransaction::new(agent_id, transaction_id),
            SuppressedStart::PreflightFailure(reason),
        );
    }

    /// Record cancellation unless a preflight failure already owns the
    /// reaction.
    pub(super) fn suppress_start_for_cancellation(
        &mut self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
    ) {
        let reaction = self
            .suppressed_starts
            .entry(CompactionTransaction::new(agent_id, transaction_id))
            .or_insert(SuppressedStart::Cancelled);
        if matches!(reaction, SuppressedStart::PreflightFailure(_)) {
            return;
        }
        *reaction = SuppressedStart::Cancelled;
    }

    /// Record that a terminal is already paired with the start without
    /// replacing a stronger preflight or cancellation reaction.
    pub(super) fn suppress_start_for_queued_terminal(
        &mut self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
    ) {
        self.suppressed_starts
            .entry(CompactionTransaction::new(agent_id, transaction_id))
            .or_insert(SuppressedStart::TerminalAlreadyQueued);
    }

    /// Consume the exclusive suppressed-start reaction for one committed start.
    pub(super) fn take_suppressed_start(
        &mut self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
    ) -> Option<SuppressedStart> {
        self.suppressed_starts
            .remove(&CompactionTransaction::new(agent_id, transaction_id))
    }

    /// Remove a stale suppressed-start reaction after a committed terminal.
    pub(super) fn remove_suppressed_start(
        &mut self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
    ) {
        self.suppressed_starts
            .remove(&CompactionTransaction::new(agent_id, transaction_id));
    }

    /// Return whether no committed-start reaction remains suppressed.
    #[cfg(test)]
    pub(super) fn suppressed_starts_is_empty(&self) -> bool {
        self.suppressed_starts.is_empty()
    }

    /// Record UI delivery ownership for one durably started transaction.
    pub(super) fn record_ui_start(
        &mut self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
        request_id: tau_proto::CompactionRequestId,
    ) {
        self.active_manual_transactions
            .entry(CompactionTransaction::new(agent_id, transaction_id))
            .or_insert(ManualCompactionStartOwner::Ui { request_id });
    }

    /// Record model-tool delivery ownership unless replay already restored it.
    pub(super) fn record_model_tool_start(
        &mut self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
        pending: PendingManualCompactionTool,
    ) {
        self.active_manual_transactions
            .entry(CompactionTransaction::new(agent_id, transaction_id))
            .or_insert(ManualCompactionStartOwner::ModelTool(pending));
    }

    /// Return whether an open UI transaction belongs to `agent_id`.
    pub(super) fn has_ui_start_for_agent(&self, agent_id: &tau_proto::AgentId) -> bool {
        self.active_manual_transactions.iter().any(|(key, owner)| {
            key.belongs_to(agent_id) && matches!(owner, ManualCompactionStartOwner::Ui { .. })
        })
    }

    /// Count open model-tool transactions belonging to `caller_agent_id`.
    pub(super) fn model_tool_start_count_for_caller(&self, caller_agent_id: &str) -> usize {
        self.active_manual_transactions
            .values()
            .filter(|owner| {
                matches!(
                    owner,
                    ManualCompactionStartOwner::ModelTool(pending)
                        if pending.caller_agent_id.as_str() == caller_agent_id
                )
            })
            .count()
    }

    /// Remove and return model-tool ownership for the named transaction.
    pub(super) fn take_model_tool_start(
        &mut self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
    ) -> Option<PendingManualCompactionTool> {
        let key = CompactionTransaction::new(agent_id, transaction_id);
        if !matches!(
            self.active_manual_transactions.get(&key),
            Some(ManualCompactionStartOwner::ModelTool(_))
        ) {
            return None;
        }
        match self.active_manual_transactions.remove(&key) {
            Some(ManualCompactionStartOwner::ModelTool(pending)) => Some(pending),
            Some(ManualCompactionStartOwner::Ui { .. }) | None => unreachable!("owner rechecked"),
        }
    }

    /// Remove UI ownership for the named transaction without consuming a model
    /// owner.
    pub(super) fn remove_ui_start(
        &mut self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
    ) {
        let key = CompactionTransaction::new(agent_id, transaction_id);
        if matches!(
            self.active_manual_transactions.get(&key),
            Some(ManualCompactionStartOwner::Ui { .. })
        ) {
            self.active_manual_transactions.remove(&key);
        }
    }

    /// Find a cloned model-tool owner by its tool-call correlation.
    pub(super) fn model_tool_start_by_call(
        &self,
        call_id: &ToolCallId,
    ) -> Option<(
        tau_proto::CompactionTransactionId,
        PendingManualCompactionTool,
    )> {
        self.active_manual_transactions
            .iter()
            .find_map(|(key, owner)| match owner {
                ManualCompactionStartOwner::ModelTool(pending) if pending.call_id == *call_id => {
                    Some((key.transaction_id().clone(), pending.clone()))
                }
                ManualCompactionStartOwner::Ui { .. }
                | ManualCompactionStartOwner::ModelTool(_) => None,
            })
    }

    /// Return whether a model-tool owner uses `call_id`.
    pub(super) fn has_model_tool_start_for_call(&self, call_id: &ToolCallId) -> bool {
        self.model_tool_start_by_call(call_id).is_some()
    }

    /// Return whether no started manual transaction retains a delivery owner.
    #[cfg(test)]
    pub(super) fn active_manual_starts_is_empty(&self) -> bool {
        self.active_manual_transactions.is_empty()
    }

    /// Return the number of started manual transactions with delivery owners.
    #[cfg(test)]
    pub(super) fn active_manual_start_count(&self) -> usize {
        self.active_manual_transactions.len()
    }

    /// Return whether the named transaction has UI delivery ownership.
    #[cfg(test)]
    pub(super) fn has_ui_start(
        &self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
    ) -> bool {
        matches!(
            self.active_manual_transactions
                .get(&CompactionTransaction::new(agent_id, transaction_id)),
            Some(ManualCompactionStartOwner::Ui { .. })
        )
    }

    /// Return the UI request correlated with the named transaction, if any.
    #[cfg(test)]
    pub(super) fn ui_start_request(
        &self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
    ) -> Option<&tau_proto::CompactionRequestId> {
        match self
            .active_manual_transactions
            .get(&CompactionTransaction::new(agent_id, transaction_id))
        {
            Some(ManualCompactionStartOwner::Ui { request_id }) => Some(request_id),
            Some(ManualCompactionStartOwner::ModelTool(_)) | None => None,
        }
    }

    /// Return whether the named transaction has model-tool delivery ownership.
    #[cfg(test)]
    pub(super) fn has_model_tool_start(
        &self,
        agent_id: tau_proto::AgentId,
        transaction_id: tau_proto::CompactionTransactionId,
    ) -> bool {
        matches!(
            self.active_manual_transactions
                .get(&CompactionTransaction::new(agent_id, transaction_id)),
            Some(ManualCompactionStartOwner::ModelTool(_))
        )
    }

    /// Remove model-tool owners while preserving independent UI coalescing
    /// state.
    pub(super) fn clear_model_tool_starts(&mut self) {
        self.active_manual_transactions
            .retain(|_, owner| matches!(owner, ManualCompactionStartOwner::Ui { .. }));
    }
}
