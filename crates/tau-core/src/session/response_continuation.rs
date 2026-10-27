//! Durable response-level continuation authority, independent of message phase.

use super::*;

impl AgentTree {
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
    fn response_continuation_dispatch_is_selected(&self, dispatch: &InferenceDispatchFold) -> bool {
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
