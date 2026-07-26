//! Mutable discovery state for one correlated agent initialization.

use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};

/// Mutable discovery state isolated to one correlated agent initialization.
#[derive(Clone)]
pub(crate) struct PendingAgentDiscovery {
    /// Exact load attempt that owns this state.
    pub(crate) initialization_id: tau_proto::AgentInitializationId,
    /// Candidate sets seeded from the session baseline and replaced per source.
    pub(crate) skill_candidates:
        std::collections::HashMap<tau_proto::SkillName, Vec<DiscoveredSkill>>,
    /// Effective winners derived atomically from `skill_candidates`.
    pub(crate) skills: std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    /// Ordered AGENTS.md files seeded from the baseline and replaced per
    /// source.
    pub(crate) agents_files: Vec<DiscoveredAgentsFile>,
    /// Captured providers that have not acknowledged this initialization.
    pub(crate) waiting_on: std::collections::HashSet<tau_proto::ConnectionId>,
}
