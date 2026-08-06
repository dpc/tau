//! Main-loop-owned supervision state for best-effort provider prewarms.

mod abort;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

pub(crate) use abort::PrewarmAbort;

/// Identity used to suppress duplicate work for one provider cache bucket.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PrewarmKey {
    /// Configured provider namespace.
    pub(crate) provider: tau_proto::ProviderName,
    /// Durable target agent whose provider cache is being warmed.
    pub(crate) agent_id: tau_proto::AgentId,
    /// Lifecycle correlation for scheduler work; absent for legacy prewarm.
    pub(crate) refresh_id: Option<tau_proto::ProviderCacheRefreshId>,
}

/// Main-loop-owned registry of bounded prewarm workers.
#[derive(Default)]
pub(crate) struct PrewarmSupervisor {
    /// Active work keyed by the exact provider cache owner.
    active: HashMap<PrewarmKey, ActivePrewarm>,
    /// Monotonic identity preventing stale completions from removing
    /// successors.
    next_generation: u64,
}

/// One active generation and its owned cancellation source.
struct ActivePrewarm {
    /// Exact worker generation for completion validation.
    generation: u64,
    /// Cancellation source passed into transport work.
    abort: PrewarmAbort,
}

impl PrewarmSupervisor {
    /// Reserves one cache owner, returning work identity unless already active.
    pub(crate) fn begin(&mut self, key: PrewarmKey) -> Option<(u64, PrewarmAbort)> {
        if self.active.contains_key(&key)
            || tau_provider_codex::MAX_CONCURRENT_PREWARMS <= self.active.len()
        {
            return None;
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let abort = PrewarmAbort::default();
        self.active.insert(
            key,
            ActivePrewarm {
                generation,
                abort: abort.clone(),
            },
        );
        Some((generation, abort))
    }

    /// Retires a worker only when its exact generation still owns the key.
    pub(crate) fn complete(&mut self, key: &PrewarmKey, generation: u64) {
        if self
            .active
            .get(key)
            .is_some_and(|active| active.generation == generation)
        {
            self.active.remove(key);
        }
    }

    /// Cancels active work for one cache owner so a real prompt can take over.
    pub(crate) fn cancel_key(&mut self, key: &PrewarmKey) {
        if let Some(active) = self.active.get(key) {
            active.abort.cancel();
        }
    }

    /// Synchronously invalidates one exact refresh before real work proceeds.
    pub(crate) fn cancel_refresh(&mut self, refresh_id: &tau_proto::ProviderCacheRefreshId) {
        let key = self
            .active
            .keys()
            .find_map(|key| (key.refresh_id.as_ref() == Some(refresh_id)).then(|| key.clone()));
        if let Some(key) = key
            && let Some(active) = self.active.remove(&key)
        {
            active.abort.cancel();
        }
    }

    /// Cancels active work for one configured provider namespace.
    pub(crate) fn cancel_provider(&mut self, provider: &tau_proto::ProviderName) {
        for (key, active) in &self.active {
            if &key.provider == provider {
                active.abort.cancel();
            }
        }
    }

    /// Cancels every active worker while retaining ownership until completion.
    pub(crate) fn cancel_all(&mut self) {
        for active in self.active.values() {
            active.abort.cancel();
        }
    }

    /// Returns whether every supervised worker has reported exact completion.
    pub(crate) fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}
