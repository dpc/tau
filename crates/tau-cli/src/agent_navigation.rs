//! Per-UI projection of harness-owned agent navigation state.

#[cfg(test)]
mod agent_navigation_tests;

use std::collections::{HashMap, HashSet};

pub(crate) use tau_proto::AgentNavigationMode as AgentNavigationState;

/// Return whether one harness-authored mode/runtime pair is
/// navigation-eligible.
pub(crate) fn is_navigation_eligible(
    mode: tau_proto::AgentNavigationMode,
    runtime_state: tau_proto::AgentRuntimeState,
) -> bool {
    match mode {
        tau_proto::AgentNavigationMode::Active => true,
        tau_proto::AgentNavigationMode::ActiveAuto => {
            runtime_state == tau_proto::AgentRuntimeState::Running
        }
        tau_proto::AgentNavigationMode::Suspended => false,
    }
}

/// One atomic snapshot of the facts used to route terminal input.
#[derive(Clone, Debug, Default)]
pub(crate) struct AgentNavigation {
    /// Agent ids currently loaded by the harness.
    live_agents: HashSet<String>,
    /// Modes received in complete harness-authored operational snapshots.
    modes: HashMap<String, tau_proto::AgentNavigationMode>,
    /// Latest authoritative outer-turn runtime state for each loaded agent.
    runtime_states: HashMap<String, tau_proto::AgentRuntimeState>,
}

impl AgentNavigation {
    /// Record a loaded agent without changing an existing navigation mode.
    pub(crate) fn mark_live(&mut self, agent_id: impl Into<String>) {
        self.live_agents.insert(agent_id.into());
    }

    /// Atomically apply one complete authoritative operational snapshot.
    pub(crate) fn apply_stats(
        &mut self,
        agent_id: &str,
        navigation_mode: tau_proto::AgentNavigationMode,
        runtime_state: tau_proto::AgentRuntimeState,
    ) {
        if self.live_agents.contains(agent_id) {
            self.modes.insert(agent_id.to_owned(), navigation_mode);
            self.runtime_states
                .insert(agent_id.to_owned(), runtime_state);
        }
    }

    /// Remove every navigation fact for an unloaded endpoint.
    pub(crate) fn unload(&mut self, agent_id: &str) {
        self.live_agents.remove(agent_id);
        self.modes.remove(agent_id);
        self.runtime_states.remove(agent_id);
    }

    /// Clear all session-scoped navigation facts.
    pub(crate) fn clear(&mut self) {
        self.live_agents.clear();
        self.modes.clear();
        self.runtime_states.clear();
    }

    /// Return the stored mode, using the ordinary-agent `active` default.
    pub(crate) fn mode(&self, agent_id: &str) -> tau_proto::AgentNavigationMode {
        self.modes.get(agent_id).copied().unwrap_or_default()
    }

    /// Return whether a loaded agent is an effective navigation target.
    pub(crate) fn is_active(&self, agent_id: &str) -> bool {
        self.live_agents.contains(agent_id)
            && self.modes.get(agent_id).is_some_and(|mode| {
                self.runtime_states
                    .get(agent_id)
                    .is_some_and(|runtime| is_navigation_eligible(*mode, *runtime))
            })
    }

    /// Return all effective navigation targets.
    pub(crate) fn active_agents(&self) -> HashSet<String> {
        self.live_agents
            .iter()
            .filter(|agent_id| self.is_active(agent_id))
            .cloned()
            .collect()
    }

    /// Return the number of effective navigation targets.
    pub(crate) fn active_count(&self) -> usize {
        self.live_agents
            .iter()
            .filter(|agent_id| self.is_active(agent_id))
            .count()
    }

    /// Return whether the harness currently has an agent loaded.
    pub(crate) fn is_live(&self, agent_id: &str) -> bool {
        self.live_agents.contains(agent_id)
    }

    /// Return all currently loaded agent ids.
    pub(crate) fn live_agents(&self) -> HashSet<String> {
        self.live_agents.clone()
    }
}
