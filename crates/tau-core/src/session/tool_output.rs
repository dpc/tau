//! Bounded explicit recovery over canonical output occurrences, not spool
//! files.

use super::*;

impl AgentTree {
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
    /// have materialized. A sibling foreground call or inference can defer
    /// them.
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

    /// Resolve the latest declaration of a display ID on the selected branch.
    /// The returned reference, not the display ID, identifies subsequent pages.
    #[must_use]
    pub fn tool_output_call(
        &self,
        call_id: &ToolCallId,
        head: Option<NodeId>,
    ) -> Option<tau_proto::ToolCallRef> {
        if let Some(call) = self
            .indexes
            .tool_output_calls
            .get(call_id)
            .copied()
            .filter(|call| self.tool_call_is_on_branch(*call, head))
        {
            return Some(call);
        }
        // Display IDs may be reused on another branch. Walk this branch rather
        // than letting the newest tree-global declaration hide its older owner.
        let mut current = head;
        while let Some(node_id) = current {
            let node = self.node(node_id)?;
            if let AgentEntry::AssistantResponse { output_items, .. } = &node.entry
                && let Some(item_index) = output_items.iter().position(
                    |item| matches!(item, ContextItem::ToolCall(call) if &call.call_id == call_id),
                )
            {
                let declaration = self
                    .indexes
                    .assistant_response_nodes_by_observation
                    .iter()
                    .find_map(|(observation, node)| (*node == node_id).then_some(*observation))?;
                return Some(tau_proto::ToolCallRef {
                    declaration,
                    item_index: u32::try_from(item_index).ok()?,
                });
            }
            current = node.parent_id;
        }
        None
    }

    /// Recover accepted output, including deferred frames, without journal I/O.
    /// Compaction changes provider windows but does not remove these nodes.
    /// Omitted producer bytes and expired temporary files are not
    /// reconstructed.
    pub fn tool_output_page(
        &self,
        call: tau_proto::ToolCallRef,
        head: Option<NodeId>,
        offset: usize,
    ) -> Result<tau_proto::ToolOutputPage, AgentEventValidationError> {
        if !self.tool_call_is_on_branch(call, head) {
            return Err(AgentEventValidationError::new(
                "tool output belongs to another branch",
            ));
        }
        let state = self
            .indexes
            .tool_outputs
            .get(&call)
            .ok_or_else(|| AgentEventValidationError::new("unknown tool output declaration"))?;
        let mut text = String::new();
        let mut total = 0usize;
        let mut next = offset;
        let mut page_full = false;
        for occurrence in &state.occurrences {
            let entry = self
                .node_for_durable_event_seq(*occurrence)
                .and_then(|node| self.node(node))
                .map(|node| &node.entry)
                .or_else(|| {
                    self.pending_context_inputs
                        .iter()
                        .find(|input| input.durable_event_seq == *occurrence)
                        .map(|input| input.entry.as_ref())
                })
                .or_else(|| {
                    self.pending_inference_inputs
                        .iter()
                        .find(|input| input.durable_event_seq == *occurrence)
                        .map(|input| input.entry.as_ref())
                })
                .ok_or_else(|| {
                    AgentEventValidationError::new("tool output context is unavailable")
                })?;
            let AgentEntry::UserInput { items, .. } = entry else {
                return Err(AgentEventValidationError::new(
                    "invalid tool output context",
                ));
            };
            for item in items {
                let ContextItem::Message(message) = item else {
                    continue;
                };
                for part in &message.content {
                    let ContentPart::Text { text: body, .. } = part else {
                        continue;
                    };
                    let start = offset.saturating_sub(total).min(body.len());
                    total = total.saturating_add(body.len());
                    if start == body.len() || page_full {
                        continue;
                    }
                    if !body.is_char_boundary(start) {
                        return Err(AgentEventValidationError::new(
                            "output offset splits a UTF-8 character",
                        ));
                    }
                    let mut end = (start + tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES - text.len())
                        .min(body.len());
                    while !body.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.push_str(&body[start..end]);
                    next = next.saturating_add(end - start);
                    page_full =
                        end < body.len() || text.len() == tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES;
                }
            }
        }
        if total < offset {
            return Err(AgentEventValidationError::new(
                "output offset exceeds retained output",
            ));
        }
        Ok(tau_proto::ToolOutputPage {
            call,
            text,
            offset,
            next_offset: (next < total).then_some(next),
            retained_bytes: total,
            terminal: state.terminal,
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
