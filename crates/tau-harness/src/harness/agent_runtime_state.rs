//! Agent identity, observation, and delegation runtime ownership.

use super::*;

/// Runtime state shared by agent lifecycle and coordination paths.
pub(crate) struct AgentRuntimeState {
    /// Agent identity, membership, and routing state.
    pub(crate) agent_registry: AgentRegistryState,
    /// Watch topology, delivery deduplication, and retirement state.
    pub(crate) agent_watch: AgentWatchState,
    /// Harness-owned delegate and wait tool state.
    pub(crate) subagents: SubagentToolState,
    /// Ambient runtime indicators grouped by publisher and agent.
    pub(crate) agent_runtime_indicators: HashMap<
        tau_proto::ConnectionId,
        HashMap<AgentId, std::collections::BTreeSet<tau_proto::AgentRuntimeIndicator>>,
    >,
}
