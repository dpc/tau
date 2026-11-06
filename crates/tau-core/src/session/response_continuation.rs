//! Durable response-level continuation authority, independent of message phase.

use super::*;

impl AgentTree {
    /// Return canonical ordinary continuation debt, whether an explicit
    /// response or a completed foreground tool round still owes the next
    /// sampling.
    #[must_use]
    pub fn pending_inference_continuation(
        &self,
    ) -> Option<&tau_proto::AgentInferenceDispatchStarted> {
        self.pending_response_continuation()
            .or_else(|| self.pending_tool_round_continuation())
    }

    /// Return the exact source of an unclaimed, selected, closed tool round.
    /// Tool declarations and the canonical aggregate prove completion; runtime
    /// callbacks, event metadata, and passive background notices do not.
    #[must_use]
    pub fn pending_tool_round_continuation(
        &self,
    ) -> Option<&tau_proto::AgentInferenceDispatchStarted> {
        if !self.pending_tool_rounds.is_empty() {
            return None;
        }
        let source = self.tool_round_continuation_source()?;
        let through = self.completed_tool_round_head(source)?;
        self.is_ancestor_head(through, self.head.map_or(AgentHead::Root, AgentHead::Node))
            .then_some(&source.checkpoint)
    }

    /// Restore only the exact open turn of a selected interrupted foreground
    /// round. A partially canceled round must not regain execution authority
    /// when restart repairs its remaining calls.
    #[must_use]
    pub fn interrupted_tool_round_outer_turn(&self) -> Option<tau_proto::AgentOuterTurnId> {
        let source = self.tool_round_continuation_source()?;
        let round = self.pending_tool_rounds.get(&source.response_node?)?;
        if round
            .terminal_results
            .values()
            .any(|item| matches!(item.status, ToolResultStatus::Cancelled { .. }))
        {
            return None;
        }
        self.prompt_started(&source.checkpoint.agent_prompt_id)?
            .outer_turn_id
            .clone()
    }

    /// Select the canonical source shared by unfinished and completed
    /// tool-round recovery, leaving standalone and output-length ownership
    /// untouched.
    fn tool_round_continuation_source(&self) -> Option<&InferenceDispatchFold> {
        let source = self
            .inference_dispatches
            .get(self.inference_dispatch_order.last()?)?;
        if !source.finished
            || !self.response_continuation_dispatch_is_selected(source)
            || !self.inference_checkpoint_allows_interrupted_retry(&source.checkpoint)
            || source.recovery_disposition != tau_proto::ContextRecoveryDisposition::None
            || source.output_length_disposition != tau_proto::OutputLengthDisposition::None
            || !matches!(
                source.provider_stop_reason,
                Some(
                    tau_proto::ProviderStopReason::ToolCalls
                        | tau_proto::ProviderStopReason::EndTurn
                )
            )
        {
            return None;
        }
        // A newer standalone owner retains its own recovery contract, including
        // failure. An older failure must not suppress a later ordinary round.
        match self.standalone_compaction_recovery() {
            None => {}
            Some(StandaloneCompactionRecovery::Blocked { failed, .. })
                if self
                    .compaction_transactions
                    .get(&failed.transaction_id)
                    .and_then(|transaction| transaction.failure_event_seq)
                    .is_some_and(|seq| seq.get() < source.durable_event_seq.get()) => {}
            Some(_) => return None,
        }
        Some(source)
    }

    /// Resolve a response's complete tool aggregate without relying on callback
    /// state or the provider's stop-reason label.
    pub(super) fn completed_tool_round_head(
        &self,
        source: &InferenceDispatchFold,
    ) -> Option<AgentHead> {
        let response = self.node(source.response_node?)?;
        let AgentEntry::AssistantResponse { output_items, .. } = &response.entry else {
            return None;
        };
        let mut calls = output_items.iter().filter_map(|item| match item {
            ContextItem::ToolCall(call) => Some(call),
            _ => None,
        });
        let node = self.terminal_tool_result_node_id(&calls.next()?.call_id)?;
        let aggregate = self.node(node)?;
        let AgentEntry::ToolResults { items } = &aggregate.entry else {
            return None;
        };
        (aggregate.parent_id == Some(response.id)
            && !items
                .iter()
                .any(|item| matches!(item.status, ToolResultStatus::Cancelled { .. }))
            && calls.all(|call| self.terminal_tool_result_node_id(&call.call_id) == Some(node)))
        .then_some(AgentHead::Node(node))
    }

    /// Count accepted explicit continuations on the selected branch in the
    /// current outer turn. Compaction and process restart do not reset the
    /// bound.
    #[must_use]
    pub fn response_continuation_count(&self) -> usize {
        self.inference_dispatches
            .values()
            .filter(|dispatch| {
                dispatch.provider_stop_reason == Some(tau_proto::ProviderStopReason::Continue)
                    && self.response_continuation_dispatch_is_selected(dispatch)
            })
            .count()
    }

    /// Return the open outer turn containing a selected explicit continuation.
    /// This also retains ownership across a claimed but interrupted successor.
    #[must_use]
    pub fn response_continuation_outer_turn(&self) -> Option<tau_proto::AgentOuterTurnId> {
        (self.response_continuation_count() != 0)
            .then(|| self.active_outer_turn.clone())
            .flatten()
    }

    /// Return the completed response still owed a new sampling. A committed
    /// successor checkpoint claims it even if dispatch subsequently becomes
    /// uncertain; cold restore must never repeat that ambiguous paid request.
    #[must_use]
    pub fn pending_response_continuation(
        &self,
    ) -> Option<&tau_proto::AgentInferenceDispatchStarted> {
        let source = self.inference_dispatch_order.iter().rev().find_map(|id| {
            let dispatch = self.inference_dispatches.get(id)?;
            (dispatch.provider_stop_reason == Some(tau_proto::ProviderStopReason::Continue)
                && self.response_continuation_dispatch_is_selected(dispatch))
            .then_some(dispatch)
        })?;
        let through = AgentHead::Node(source.response_node?);
        let claimed = self.inference_dispatches.values().any(|dispatch| {
            dispatch.durable_event_seq.get() > source.durable_event_seq.get()
                && self.is_ancestor_head(through, dispatch.checkpoint.through)
        });
        (!claimed).then_some(&source.checkpoint)
    }

    /// Repair the narrow crash cut after a continuation's final response
    /// commits but before its ordinary outer-turn finish. Other recovery
    /// families keep their own compaction, tool, and output-length
    /// ownership.
    #[must_use]
    pub fn response_continuation_finish_repair(&self) -> Option<tau_proto::AgentOuterTurnId> {
        let turn = self.response_continuation_outer_turn()?;
        let source = self.inference_dispatch_order.iter().find_map(|id| {
            let dispatch = self.inference_dispatches.get(id)?;
            (dispatch.provider_stop_reason == Some(tau_proto::ProviderStopReason::Continue)
                && self.response_continuation_dispatch_is_selected(dispatch))
            .then_some(dispatch)
        })?;
        let source_head = AgentHead::Node(source.response_node?);
        let latest = self.inference_dispatch_order.iter().rev().find_map(|id| {
            let dispatch = self.inference_dispatches.get(id)?;
            (dispatch.checkpoint.operation == tau_proto::PromptOperation::Inference
                && self.is_ancestor_head(source_head, dispatch.checkpoint.through)
                && self.is_ancestor_head(
                    dispatch
                        .response_node
                        .map_or(dispatch.checkpoint.through, AgentHead::Node),
                    self.head.map_or(AgentHead::Root, AgentHead::Node),
                ))
            .then_some(dispatch)
        })?;
        (latest.finished
            && !latest.interrupted_retry
            && !latest.final_status_challenged
            && (latest.cancelled_terminal_head.is_some()
                || matches!(
                    latest.provider_stop_reason,
                    Some(
                        tau_proto::ProviderStopReason::EndTurn
                            | tau_proto::ProviderStopReason::Error
                            | tau_proto::ProviderStopReason::RepetitionDetected
                            | tau_proto::ProviderStopReason::Length
                    )
                ))
            && !latest.rearms_output_length
            && latest.recovery_disposition == tau_proto::ContextRecoveryDisposition::None
            && latest.output_length_disposition == tau_proto::OutputLengthDisposition::None
            && self.pending_tool_rounds.is_empty())
        .then_some(turn)
    }

    /// Test selected durable lineage and exact open-turn membership without
    /// depending on runtime state or reconstructing provider messages.
    pub(super) fn response_continuation_dispatch_is_selected(
        &self,
        dispatch: &InferenceDispatchFold,
    ) -> bool {
        self.active_outer_turn.is_some()
            && dispatch.checkpoint.operation == tau_proto::PromptOperation::Inference
            && self
                .prompt_starts
                .get(&dispatch.checkpoint.agent_prompt_id)
                .is_some_and(|started| started.outer_turn_id == self.active_outer_turn)
            && self.is_ancestor_head(
                dispatch
                    .response_node
                    .map_or(dispatch.checkpoint.through, AgentHead::Node),
                self.head.map_or(AgentHead::Root, AgentHead::Node),
            )
    }
}
