//! Authenticated creator relationships between agents in one harness session.

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use tau_proto::{AgentCreator, AgentId, SessionId};

/// Harness-owned graph of authenticated agent creation relationships.
///
/// The graph retains relationships after an individual runtime retires, but the
/// harness clears it when the active session changes.
#[derive(Default)]
pub(crate) struct AgentCreatorTopology {
    /// One authenticated creator for each child that has one.
    creator_by_agent: HashMap<AgentId, AgentId>,
}

/// Result of accepting one immutable creation fact into an
/// [`AgentCreatorTopology`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RecordCreatorOutcome {
    /// A new authenticated same-session edge was recorded.
    Recorded,
    /// An identical fact repeated an already-recorded edge.
    AlreadyRecorded,
    /// The agent was not created by an agent in this session.
    NoCreatorEdge,
    /// The reported creator belongs to a different session.
    ForeignSession,
    /// The child attempted to name itself as its creator.
    RejectedSelf,
    /// The edge would form a cycle.
    RejectedCycle,
    /// An immutable child already has a different authenticated creator.
    Conflict {
        /// Creator retained from the first valid creation fact.
        existing_creator: AgentId,
    },
}

impl AgentCreatorTopology {
    /// Records a creator edge when `creator` authenticates an agent in
    /// `session_id`.
    ///
    /// The first valid edge wins. Repeated facts are idempotent; malformed
    /// self/cycle edges and later conflicts leave the graph unchanged.
    pub(crate) fn record(
        &mut self,
        child: AgentId,
        creator: Option<&AgentCreator>,
        session_id: &SessionId,
    ) -> RecordCreatorOutcome {
        let Some(AgentCreator::Agent {
            session_id: creator_session_id,
            agent_id: creator,
        }) = creator
        else {
            return RecordCreatorOutcome::NoCreatorEdge;
        };
        if creator_session_id != session_id {
            return RecordCreatorOutcome::ForeignSession;
        }
        if creator == &child {
            return RecordCreatorOutcome::RejectedSelf;
        }
        if let Some(existing_creator) = self.creator_by_agent.get(&child) {
            return if existing_creator == creator {
                RecordCreatorOutcome::AlreadyRecorded
            } else {
                RecordCreatorOutcome::Conflict {
                    existing_creator: existing_creator.clone(),
                }
            };
        }
        if self.creator_chain_includes(creator, &child) {
            return RecordCreatorOutcome::RejectedCycle;
        }
        self.creator_by_agent.insert(child.clone(), creator.clone());
        RecordCreatorOutcome::Recorded
    }

    /// Returns `agent` followed by every creator ancestor, nearest first.
    pub(crate) fn inclusive_creator_chain(&self, agent: &AgentId) -> Vec<AgentId> {
        let mut chain = Vec::new();
        let mut current = agent.clone();
        let mut visited = HashSet::new();
        while visited.insert(current.clone()) {
            chain.push(current.clone());
            let Some(creator) = self.creator_by_agent.get(&current) else {
                break;
            };
            current = creator.clone();
        }
        chain
    }

    /// Returns whether `start` reaches `needle` by following creator edges.
    fn creator_chain_includes(&self, start: &AgentId, needle: &AgentId) -> bool {
        let mut current = start.clone();
        let mut visited = HashSet::new();
        while visited.insert(current.clone()) {
            if &current == needle {
                return true;
            }
            let Some(creator) = self.creator_by_agent.get(&current) else {
                return false;
            };
            current = creator.clone();
        }
        false
    }
}
