//! Agent status projections shared by status-bar and completion presentation.

use super::*;

impl EventRenderer {
    /// Returns the selected-agent work phase and escaped task title.
    pub(super) fn selected_agent_work_status(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> (&'static str, Option<String>) {
        let Some(status) = self
            .watches
            .agent_stats
            .get(agent_id)
            .map(|stats| &stats.work_status)
        else {
            return (crate::list_agents::work_status_symbol(None), None);
        };
        (
            crate::list_agents::work_status_symbol(Some(status.phase())),
            status.title().map(tau_proto::visible_escape_metadata),
        )
    }
}
