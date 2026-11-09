//! Canonical streamed-output policy and residency accounting.

use super::*;

impl AgentTree {
    /// Native continuation owns these accepted occurrences, even while a
    /// sibling still holds the aggregate. They no longer authorize input wakes.
    #[must_use]
    pub fn tool_output_is_native(&self, call: tau_proto::ToolCallRef) -> bool {
        self.indexes
            .tool_outputs
            .get(&call)
            .is_some_and(|state| state.native)
    }

    /// Test occurrence identity rather than a reusable provider display ID.
    #[must_use]
    pub fn tool_output_occurrence_is_native(
        &self,
        call: tau_proto::ToolCallRef,
        seq: PersistedAgentEventSeq,
    ) -> bool {
        self.indexes
            .tool_outputs
            .get(&call)
            .is_some_and(|state| state.native && state.occurrences.contains(&seq))
    }

    /// Fold only unexposed source belonging to this exact declaration into its
    /// native terminal. Historical receipts keep the ordinary envelope route.
    pub(super) fn record_native_tool_output(
        &mut self,
        seq: PersistedAgentEventSeq,
        call_id: &ToolCallId,
        tool_type: ToolType,
        status: ToolResultStatus,
        capture: &tau_proto::ToolFinalOutput,
    ) -> Option<NodeId> {
        let call = self.indexes.tool_output_calls[call_id];
        let mut updates = Vec::new();
        self.pending_context_inputs.retain_mut(|input| {
            if let PendingToolContextEntry::Output(update) = &input.entry
                && update.call == call
            {
                updates.push((**update).clone());
                false
            } else {
                true
            }
        });
        let state = self
            .indexes
            .tool_outputs
            .get_mut(&call)
            .expect("native declaration");
        assert_eq!(
            updates.len(),
            state.occurrences.len(),
            "native source already exposed"
        );
        state.terminal = true;
        state.native = true;
        state.occurrences.push(seq);
        let round = self
            .pending_tool_rounds
            .get_mut(&self.tool_call_rounds[call_id])
            .expect("native foreground round");
        round
            .native_source_occurrences
            .extend_from_slice(&state.occurrences);
        self.record_terminal_tool_result(ToolResultItem {
            call_id: call_id.clone(),
            tool_type,
            status,
            output: tau_proto::ToolResponse::from_captured_output(&updates, capture),
            presentation: tau_proto::ToolResultPresentation::NativeCapturedOutput,
            provider_content: Vec::new(),
        })
    }

    /// Read the durable declaration's output policy, without consulting live
    /// tools.
    #[must_use]
    pub fn tool_output_is_streamed(&self, call: tau_proto::ToolCallRef) -> bool {
        self.indexes
            .tool_outputs
            .get(&call)
            .is_some_and(|state| state.streamed)
    }

    /// Output custody ends only after the terminal and all its source context
    /// have materialized, including context deferred by a sibling or inference.
    #[must_use]
    pub fn tool_output_needs_residency(&self, call: tau_proto::ToolCallRef) -> bool {
        self.indexes.tool_outputs.get(&call).is_some_and(|state| {
            state.streamed
                && (!state.terminal
                    || state
                        .occurrences
                        .iter()
                        .any(|seq| self.node_for_durable_event_seq(*seq).is_none()))
        })
    }

    /// Reconstruct cold repair debt, never processes. Foreground repair is
    /// tree-global; background repair follows the selected branch. Historical
    /// off-branch placeholders with no deferred bytes do not own live capacity.
    /// Accepted deferred source occurrences remain debt on every branch.
    pub fn pending_tool_output_residencies(
        &self,
    ) -> impl Iterator<Item = tau_proto::ToolCallRef> + '_ {
        self.indexes.tool_outputs.iter().filter_map(|(call, state)| {
            if !state.streamed { return None; }
            if state.occurrences.iter().any(|seq| self.node_for_durable_event_seq(*seq).is_none()) {
                return Some(*call);
            }
            if state.terminal { return None; }
            let foreground = self.indexes.assistant_response_nodes_by_observation
                .get(&call.declaration)
                .and_then(|node| self.pending_tool_rounds.get(node).zip(self.node(*node)))
                .is_some_and(|(round, node)| {
                    let AgentEntry::AssistantResponse { output_items, .. } = &node.entry
                    else { return false };
                    matches!(output_items.get(call.item_index as usize), Some(ContextItem::ToolCall(item))
                        if !round.terminal_results.contains_key(&item.call_id))
                });
            (foreground || self.tool_call_is_on_branch(*call, self.head)).then_some(*call)
        })
    }

    /// Native nonstreaming terminals also fence later incremental reports.
    pub(super) fn close_tool_output(&mut self, call_id: &ToolCallId) {
        if let Some(call) = self.indexes.tool_output_calls.get(call_id)
            && let Some(state) = self.indexes.tool_outputs.get_mut(call)
        {
            state.terminal = true;
        }
    }
}
