//! Transient provider-native tool rows, separate from Tau tool runtime state.

use super::*;

/// Renders a provider-native tool through the generic tool state while keeping
/// the execution-boundary qualifier as a subdued informational segment.
pub(crate) fn provider_native_tool_display(
    update: &tau_proto::ProviderNativeToolStatusUpdate,
) -> ToolCallDisplay {
    let mut display = render_tool_use_state(&update.tool_name, &update.display);
    display.leading_segments.insert(
        0,
        ToolLineSegment {
            text: "(native)".to_owned(),
            status: ToolStatus::Info,
            no_leading_space: false,
        },
    );
    display
}

impl EventRenderer {
    /// Projects one transient provider-native lifecycle through the generic
    /// tool renderer without creating Tau tool runtime state or counters.
    pub(super) fn handle_provider_native_tool(
        &mut self,
        prompt_id: &tau_proto::AgentPromptId,
        update: &tau_proto::ProviderNativeToolStatusUpdate,
    ) {
        let key = (prompt_id.clone(), update.call_id.clone());
        let display = provider_native_tool_display(update);
        match update.phase {
            tau_proto::ProviderNativeToolPhase::Started => {
                let live_block = self.render_live_tool_block(&display);
                if let Some(state) = self.transcript.runtime.native_tool_calls.get_mut(&key) {
                    // Later provider metadata enriches this call without
                    // resetting its lifetime or adding
                    // another live/history row.
                    if let Some(block_id) = state.block_id {
                        self.resources.handle.set_block(block_id, live_block);
                    }
                    state.live_display = Some(display);
                    self.resources.handle.redraw();
                    return;
                }
                let live_id = self.resources.handle.new_block(
                    format!(
                        "provider-native-tool-live:{}:{}:{}",
                        prompt_id, update.tool_name, update.call_id
                    ),
                    live_block,
                );
                self.resources
                    .handle
                    .push_above_active_before_any(live_id, self.non_tool_activity_anchor_ids());
                let history_id = self.resources.handle.new_block(
                    format!(
                        "provider-native-tool-history:{}:{}:{}",
                        prompt_id, update.tool_name, update.call_id
                    ),
                    Self::empty_block(),
                );
                self.resources.handle.push_history(history_id);
                self.transcript.runtime.native_tool_calls.insert(
                    key,
                    ToolCallState {
                        block_id: Some(live_id),
                        history_block_id: Some(history_id),
                        live_display: Some(display),
                        started_at: Some(Instant::now()),
                        ..ToolCallState::default()
                    },
                );
                self.resources.handle.redraw();
            }
            tau_proto::ProviderNativeToolPhase::Completed => {
                let prior = self.transcript.runtime.native_tool_calls.remove(&key);
                if let Some(block_id) = prior.as_ref().and_then(|state| state.block_id) {
                    self.resources.handle.remove_block(block_id);
                }
                self.record_tool_result_block(
                    prior.and_then(|state| state.history_block_id),
                    display,
                    None,
                );
                self.resources.handle.redraw();
            }
        }
    }

    /// Removes unfinished transient native-tool rows owned by a terminal prompt
    /// without synthesizing completion or durable tool history.
    pub(super) fn retire_provider_native_tools_for_prompt(
        &mut self,
        prompt_id: &tau_proto::AgentPromptId,
    ) {
        let keys = self
            .transcript
            .runtime
            .native_tool_calls
            .keys()
            .filter(|(owner, _)| owner == prompt_id)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(state) = self.transcript.runtime.native_tool_calls.remove(&key) {
                if let Some(block_id) = state.block_id {
                    self.resources.handle.remove_block(block_id);
                }
                if let Some(block_id) = state.history_block_id {
                    self.resources.handle.remove_block(block_id);
                }
            }
        }
    }
}
