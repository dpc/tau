//! Runtime-only self and authenticated-creator-subtree cost accounting.

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use tau_proto::{AgentId, EstimatedApiCost};

use crate::agent_creator_topology::AgentCreatorTopology;

/// Runtime-only estimated-cost totals keyed by stable agent identity.
#[derive(Default)]
pub(crate) struct AgentCostLedger {
    /// Costs directly incurred by each agent in this harness session.
    self_costs: HashMap<AgentId, EstimatedApiCost>,
    /// Inclusive self-plus-authenticated-descendant costs for each agent.
    creator_subtree_costs: HashMap<AgentId, EstimatedApiCost>,
}

impl AgentCostLedger {
    /// Adds one accepted provider-response increment and returns every changed
    /// id.
    pub(crate) fn add_increment(
        &mut self,
        agent_id: &AgentId,
        increment: EstimatedApiCost,
        topology: &AgentCreatorTopology,
    ) -> Vec<AgentId> {
        Self::saturating_add(&mut self.self_costs, agent_id, increment);
        let changed = topology.inclusive_creator_chain(agent_id);
        for changed_agent_id in &changed {
            Self::saturating_add(&mut self.creator_subtree_costs, changed_agent_id, increment);
        }
        changed
    }

    /// Propagates a pre-existing child subtree once after a newly recorded
    /// edge.
    pub(crate) fn attach_existing_subtree(
        &mut self,
        child: &AgentId,
        topology: &AgentCreatorTopology,
    ) {
        let subtree = self.creator_subtree_cost(child);
        for creator in topology.inclusive_creator_chain(child).into_iter().skip(1) {
            Self::saturating_add(&mut self.creator_subtree_costs, &creator, subtree);
        }
    }

    /// Returns the self-only runtime cost for an agent.
    pub(crate) fn self_cost(&self, agent_id: &AgentId) -> EstimatedApiCost {
        self.self_costs.get(agent_id).copied().unwrap_or_default()
    }

    /// Returns the inclusive authenticated-creator-subtree cost for an agent.
    pub(crate) fn creator_subtree_cost(&self, agent_id: &AgentId) -> EstimatedApiCost {
        self.creator_subtree_costs
            .get(agent_id)
            .copied()
            .unwrap_or_default()
    }

    /// Saturating-adds `increment` to one total.
    fn saturating_add(
        costs: &mut HashMap<AgentId, EstimatedApiCost>,
        agent_id: &AgentId,
        increment: EstimatedApiCost,
    ) {
        let cost = costs.entry(agent_id.clone()).or_default();
        *cost = EstimatedApiCost::from_picodollars(
            cost.as_picodollars()
                .saturating_add(increment.as_picodollars()),
        );
    }
}
