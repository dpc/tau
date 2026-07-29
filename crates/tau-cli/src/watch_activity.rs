//! Exact recursive activity projection for the current live agent-watch DAG.

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};

/// Directed watch edge `(watcher, watched)`.
pub(crate) type WatchEdge = (String, String);

/// Derived recursive activity over the current live watch topology.
#[derive(Debug, Default)]
pub(crate) struct WatchActivityProjection {
    /// Watchers that own or can reach a directly running watch edge.
    active_watchers: HashSet<String>,
    /// Watch targets effective for the session-wide side-agent count.
    effective_targets: HashSet<String>,
    /// Current edge-scoped direct-running facts.
    direct_edges: HashMap<String, HashSet<String>>,
}

impl WatchActivityProjection {
    /// Computes exact activity by flooding from direct-running edge owners to
    /// their ancestors through the reverse watch index.
    pub(crate) fn new(
        watched_agents: &HashMap<String, Vec<String>>,
        agent_watchers: &HashMap<String, Vec<String>>,
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
        // that direct facts must belong to live edges and catches bad callers in
        // debug builds without expanding the production projection.
        debug_assert!(direct_edges.iter().all(|(watcher, watched)| {
            watched_agents
                .get(watcher)
                .is_some_and(|targets| targets.contains(watched))
        }));

        let direct_edges = direct_edges.into_iter().fold(
            HashMap::<String, HashSet<String>>::new(),
            |mut by_watcher, (watcher, watched)| {
                by_watcher.entry(watcher).or_default().insert(watched);
                by_watcher
            },
        );
        Self {
            active_watchers,
            effective_targets,
            direct_edges,
        }
    }

    /// Returns whether a direct watch edge is itself running.
    pub(crate) fn edge_is_directly_running(&self, watcher: &str, watched: &str) -> bool {
        self.direct_edges
            .get(watcher)
            .is_some_and(|targets| targets.contains(watched))
    }

    /// Returns whether an agent watches a directly or recursively active
    /// target.
    pub(crate) fn watcher_is_active(&self, watcher: &str) -> bool {
        self.active_watchers.contains(watcher)
    }

    /// Returns the unique recursively effective targets for global counting.
    pub(crate) fn effective_targets(&self) -> &HashSet<String> {
        &self.effective_targets
    }

    /// Finds the nearest directly running descendant, breaking equal-depth ties
    /// by stable agent id.
    pub(crate) fn witness_for<'a>(
        &self,
        root: &'a str,
        watched_agents: &'a HashMap<String, Vec<String>>,
    ) -> Option<String> {
        let mut visited = HashSet::from([root]);
        let mut level = vec![root];
        while !level.is_empty() {
            let mut witnesses = Vec::new();
            let mut next = Vec::new();
            for watcher in level {
                // ast-grep-ignore: filter-in-loop
                for child in watched_agents
                    .get(watcher)
                    .into_iter()
                    .flatten()
                    .map(String::as_str)
                {
                    if self.edge_is_directly_running(watcher, child) {
                        witnesses.push(child);
                    } else if visited.insert(child) {
                        next.push(child);
                    }
                }
            }
            if let Some(witness) = witnesses.into_iter().min() {
                return Some(witness.to_owned());
            }
            level = next;
        }
        None
    }
}
