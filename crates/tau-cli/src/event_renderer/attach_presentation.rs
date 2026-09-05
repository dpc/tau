//! Attach-only roster presentation and attachment-local intent access.

use super::{EventRenderer, render_action_output_block, selection_intent};

impl EventRenderer {
    /// Adds metadata-only non-interactive members to the attachment overview.
    pub(crate) fn show_attach_roster(&mut self, entries: &[tau_proto::SessionAgentListEntry]) {
        if self.selection.displayed_agent_id.is_some() {
            self.update_hidden_no_agent_state(|this| this.append_attach_roster(entries));
        } else {
            self.append_attach_roster(entries);
        }
    }

    /// Appends roster metadata to the currently restored overview snapshot.
    fn append_attach_roster(&mut self, entries: &[tau_proto::SessionAgentListEntry]) {
        for entry in entries {
            let lifecycle = match entry.lifecycle {
                tau_proto::SessionAgentLifecycle::Live { .. } => continue,
                tau_proto::SessionAgentLifecycle::Unavailable => "unavailable",
                tau_proto::SessionAgentLifecycle::Unloaded => "unloaded",
            };
            let reason = match entry.facts {
                tau_proto::SessionAgentFacts::Available { .. } => "metadata only",
                tau_proto::SessionAgentFacts::Missing => "creation facts missing",
                tau_proto::SessionAgentFacts::Invalid => "creation facts invalid",
                tau_proto::SessionAgentFacts::Unreadable => "creation facts unreadable",
            };
            self.resources.handle.print_output(
                "attach-roster",
                render_action_output_block(
                    &self.resources.theme,
                    &format!(
                        "@{}  {lifecycle} — {reason}; non-interactive",
                        entry.agent_id
                    ),
                ),
            );
            self.transcript.ownership.contains_overview_message = true;
            self.transcript.ownership.preserve_on_fresh_agent_switch = true;
        }
    }

    /// Reports a bounded requester-directed roster failure without changing
    /// attach selection or loading any agent.
    pub(crate) fn show_attach_roster_error(&mut self, error: &str) {
        if self.selection.displayed_agent_id.is_some() {
            self.update_hidden_no_agent_state(|this| this.append_attach_roster_error(error));
        } else {
            self.append_attach_roster_error(error);
        }
    }

    /// Appends one roster error to the currently restored overview snapshot.
    fn append_attach_roster_error(&mut self, error: &str) {
        self.resources.handle.print_output(
            "attach-roster-error",
            render_action_output_block(
                &self.resources.theme,
                &format!("attach roster unavailable: {error}"),
            ),
        );
        self.transcript.ownership.contains_overview_message = true;
        self.transcript.ownership.preserve_on_fresh_agent_switch = true;
    }

    /// Returns the shared attachment-local input and selection authority.
    pub(crate) fn current_agent_state(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<selection_intent::SelectionIntent>> {
        self.selection.current_agent_state.clone()
    }

    #[cfg(test)]
    /// Returns the transcript currently materialized by the renderer.
    pub(crate) fn displayed_agent_id_for_test(&self) -> Option<&tau_proto::AgentId> {
        self.selection.displayed_agent_id.as_ref()
    }
}
