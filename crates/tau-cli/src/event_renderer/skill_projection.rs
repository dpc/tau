//! Frozen skill completion and initialization-summary projections.

use super::*;

impl EventRenderer {
    /// Print one initialization discovery summary into the active transcript.
    pub(super) fn print_agent_context_initialized(
        &mut self,
        initialized: &tau_proto::HarnessAgentContextInitialized,
    ) {
        self.retain_diagnostic_block(
            "agent-context-initialized",
            tau_proto::NoticeLevel::Info,
            DiagnosticProjection::AgentContextInitialized {
                event: initialized.clone(),
                unadvertised_count: self
                    .resources
                    .skill_state
                    .unadvertised_count(&initialized.listed_skills),
            },
        );
    }

    /// Prospective completion uses the configured group, never name-prefix
    /// inference; selected-agent completion remains its frozen projection.
    pub(super) fn refresh_skill_prospective_role(&self) {
        let role = self.role.current_role.clone().unwrap_or_default();
        let group = self
            .role
            .role_groups_available
            .lock()
            .ok()
            .and_then(|groups| {
                groups
                    .iter()
                    .find(|group| group.roles.contains(&role))
                    .map(|group| group.name.clone())
            })
            .unwrap_or_else(|| role.clone());
        self.resources.skill_state.set_prospective_role(role, group);
    }
}
