//! Cycle-safe watched-row selection and recursive activity projection.

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};

/// Directed watch edge `(watcher, watched)`.
pub(crate) type WatchEdge = (tau_proto::AgentId, tau_proto::AgentId);

/// One visible row selected from the watched-agent graph.
pub(crate) struct VisibleWatchRow {
    /// Stable watched-agent identity.
    pub(crate) agent_id: tau_proto::AgentId,
    /// Shortest distance from the viewed root agent.
    pub(crate) depth: usize,
    /// Deterministic immediate predecessor for an indirect row.
    pub(crate) via: Option<tau_proto::AgentId>,
}

/// Largest visible recursive closure rendered before direct-only fallback.
pub(crate) const VISIBLE_WATCH_EXPANSION_LIMIT: usize = 8;

/// Derived row selection and recursive activity over the current watch graph.
#[derive(Debug, Default)]
pub(crate) struct WatchGraphProjection {
    /// Watchers that own or can reach a directly running watch edge.
    #[cfg(test)]
    active_watchers: HashSet<tau_proto::AgentId>,
    /// Watch targets effective for the session-wide side-agent count.
    effective_targets: HashSet<tau_proto::AgentId>,
    /// Current edge-scoped direct-running facts.
    direct_edges: HashMap<tau_proto::AgentId, HashSet<tau_proto::AgentId>>,
}

impl WatchGraphProjection {
    /// Computes exact activity by flooding from direct-running edge owners to
    /// their ancestors through the reverse watch index.
    pub(crate) fn new(
        watched_agents: &HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>>,
        agent_watchers: &HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>>,
        direct_edges: HashSet<WatchEdge>,
    ) -> Self {
        let mut active_watchers = HashSet::new();
        let mut effective_targets = HashSet::new();
        let mut queue = VecDeque::new();

        for (watcher, watched) in &direct_edges {
            effective_targets.insert(watched.clone());
            if active_watchers.insert(watcher.clone()) {
                queue.push_back(watcher.clone());
            }
        }
        while let Some(active_watcher) = queue.pop_front() {
            for parent in agent_watchers.get(&active_watcher).into_iter().flatten() {
                if active_watchers.insert(parent.clone()) {
                    queue.push_back(parent.clone());
                }
            }
        }
        effective_targets.extend(
            active_watchers
                .iter()
                .filter(|agent_id| agent_watchers.contains_key(*agent_id))
                .cloned(),
        );

        // Keep the forward topology in the constructor contract: it documents
        // that direct facts must belong to live edges and catches bad callers
        // in debug builds without expanding the production projection.
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(direct_edges.iter().all(|(watcher, watched)| {
            watched_agents
                .get(watcher)
                .is_some_and(|targets| targets.contains(watched))
        }));

        let direct_edges = direct_edges.into_iter().fold(
            HashMap::<tau_proto::AgentId, HashSet<tau_proto::AgentId>>::new(),
            |mut by_watcher, (watcher, watched)| {
                by_watcher.entry(watcher).or_default().insert(watched);
                by_watcher
            },
        );
        Self {
            #[cfg(test)]
            active_watchers,
            effective_targets,
            direct_edges,
        }
    }

    /// Returns whether a direct watch edge is itself running.
    pub(crate) fn edge_is_directly_running(
        &self,
        watcher: &tau_proto::AgentId,
        watched: &tau_proto::AgentId,
    ) -> bool {
        self.direct_edges
            .get(watcher)
            .is_some_and(|targets| targets.contains(watched))
    }

    /// Returns whether an agent watches a directly or recursively active
    /// target.
    #[cfg(test)]
    pub(crate) fn watcher_is_active(&self, watcher: &tau_proto::AgentId) -> bool {
        self.active_watchers.contains(watcher)
    }

    /// Returns the unique recursively effective targets for global counting.
    pub(crate) fn effective_targets(&self) -> &HashSet<tau_proto::AgentId> {
        &self.effective_targets
    }

    /// Selects a cycle-safe, deduplicated visible closure from one viewed root.
    ///
    /// Full expansion uses shortest paths with lexicographic path ties and
    /// returns `(depth, agent_id)` order. Once the visible closure exceeds
    /// `expansion_limit`, it falls back to every visible direct watch without
    /// truncating that direct set.
    pub(crate) fn visible_rows(
        root: &tau_proto::AgentId,
        watched_agents: &HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>>,
        visible: impl Fn(&tau_proto::AgentId) -> bool,
        expansion_limit: usize,
    ) -> Vec<VisibleWatchRow> {
        let mut direct = watched_agents
            .get(root)
            .into_iter()
            .flatten()
            .filter(|agent_id| *agent_id != root)
            .cloned()
            .collect::<Vec<_>>();
        direct.sort();
        direct.dedup();
        let direct_rows = direct
            .iter()
            .filter(|agent_id| visible(agent_id))
            .map(|agent_id| VisibleWatchRow {
                agent_id: agent_id.clone(),
                depth: 1,
                via: None,
            })
            .collect::<Vec<_>>();

        let mut visited = HashSet::from([root.clone()]);
        let mut level = direct
            .into_iter()
            .map(|agent_id| (agent_id, root.clone()))
            .collect::<Vec<_>>();
        let mut rows = Vec::new();
        let mut depth = 1;
        while !level.is_empty() {
            let mut next = Vec::new();
            let mut next_ids = HashSet::new();
            // `level` is lexicographic path order: parents retain the previous
            // level's order and each sorted child set extends one common
            // prefix. The first candidate for a shared child
            // therefore owns its stable equal-depth predecessor.
            for (agent_id, predecessor) in level {
                if !visited.insert(agent_id.clone()) {
                    continue;
                }
                if visible(&agent_id) {
                    rows.push(VisibleWatchRow {
                        agent_id: agent_id.clone(),
                        depth,
                        via: (1 < depth).then_some(predecessor),
                    });
                    if expansion_limit < rows.len() {
                        return direct_rows;
                    }
                }

                let mut children = watched_agents
                    .get(&agent_id)
                    .into_iter()
                    .flatten()
                    .filter(|child| !visited.contains(*child))
                    .cloned()
                    .collect::<Vec<_>>();
                children.sort();
                children.dedup();
                for child in children {
                    if next_ids.insert(child.clone()) {
                        next.push((child, agent_id.clone()));
                    }
                }
            }
            level = next;
            depth += 1;
        }
        rows.sort_by(|left, right| {
            (left.depth, &left.agent_id).cmp(&(right.depth, &right.agent_id))
        });
        rows
    }

    /// Finds the nearest directly running descendant, breaking equal-depth ties
    /// by stable agent id.
    pub(crate) fn witness_for<'a>(
        &self,
        root: &'a tau_proto::AgentId,
        watched_agents: &'a HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>>,
    ) -> Option<tau_proto::AgentId> {
        let mut visited = HashSet::from([root]);
        let mut level = vec![root];
        while !level.is_empty() {
            let mut witnesses = Vec::new();
            let mut next = Vec::new();
            for watcher in level {
                for child in watched_agents.get(watcher).into_iter().flatten() {
                    if self.edge_is_directly_running(watcher, child) {
                        witnesses.push(child);
                    } else if visited.insert(child) {
                        next.push(child);
                    }
                }
            }
            if let Some(witness) = witnesses.into_iter().min() {
                return Some(witness.clone());
            }
            level = next;
        }
        None
    }
}
