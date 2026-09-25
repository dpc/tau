//! Immutable discovery surface for one initialized agent.

use crate::discovery::DiscoveredSkill;

/// Immutable effective discovery surface frozen for one initialized agent.
#[derive(Clone)]
pub(crate) struct FrozenAgentDiscovery {
    /// Raw source inputs retained independently of filtered effective winners.
    pub(crate) inputs: crate::pending_agent_discovery::PendingAgentDiscovery,
    /// Exact load attempt represented by the snapshot.
    pub(crate) initialization_id: tau_proto::AgentInitializationId,
    /// Effective skills used by prompt and tool lookup.
    pub(crate) skills: std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
}
