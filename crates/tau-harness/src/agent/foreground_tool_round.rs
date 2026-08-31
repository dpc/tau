//! Owns provider-ordered live membership for one foreground tool round.

use std::collections::HashSet;

use tau_proto::ToolCallId;

/// Runtime membership for one foreground tool round.
///
/// Provider order is immutable. Terminal settlement removes exact live
/// membership in constant time, while the uncommon cancellation and preemption
/// projections recover the surviving calls in provider order.
#[derive(Clone, Debug)]
pub(crate) struct ForegroundToolRound {
    /// Tool call identifiers in immutable provider order.
    provider_order: Vec<ToolCallId>,
    /// Exact identifiers that have not settled yet.
    live: HashSet<ToolCallId>,
    /// Membership probes performed by terminal settlement in tests.
    #[cfg(test)]
    completion_work: usize,
}

/// Runtime equality intentionally excludes the test-only work counter.
impl PartialEq for ForegroundToolRound {
    fn eq(&self, other: &Self) -> bool {
        self.provider_order == other.provider_order && self.live == other.live
    }
}

impl Eq for ForegroundToolRound {}

impl ForegroundToolRound {
    /// Creates a round from tool calls in provider order.
    pub(crate) fn new(provider_order: Vec<ToolCallId>) -> Self {
        let live = provider_order.iter().cloned().collect();
        Self {
            provider_order,
            live,
            #[cfg(test)]
            completion_work: 0,
        }
    }

    /// Settles one live call and returns whether this settlement closed the
    /// round.
    ///
    /// Unknown, duplicate, and late identifiers leave the round unchanged.
    pub(crate) fn complete(&mut self, call_id: &str) -> bool {
        self.remove_live(call_id) && self.live.is_empty()
    }

    /// Returns whether no call remains live.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Returns the sole live call, if exactly one remains.
    pub(crate) fn sole_remaining(&self) -> Option<&ToolCallId> {
        (self.live.len() == 1)
            .then(|| {
                self.provider_order
                    .iter()
                    .find(|call_id| self.live.contains(*call_id))
            })
            .flatten()
    }

    /// Projects live calls in their immutable provider order.
    pub(crate) fn ordered_remaining(&self) -> Vec<ToolCallId> {
        self.provider_order
            .iter()
            .filter(|call_id| self.contains_live(call_id))
            .cloned()
            .collect()
    }

    /// Removes one identifier through the exact live-membership index.
    fn remove_live(&mut self, call_id: &str) -> bool {
        #[cfg(test)]
        {
            self.completion_work += 1;
        }
        self.live.remove(call_id)
    }

    /// Checks one provider-order entry against exact live membership.
    fn contains_live(&self, call_id: &ToolCallId) -> bool {
        self.live.contains(call_id)
    }

    /// Adds a deliberately stale runtime-only identifier for repair tests.
    #[cfg(test)]
    pub(crate) fn push(&mut self, call_id: ToolCallId) {
        if self.live.insert(call_id.clone()) {
            self.provider_order.push(call_id);
        }
    }

    /// Returns terminal-settlement membership work for deterministic tests.
    #[cfg(test)]
    pub(crate) fn completion_work(&self) -> usize {
        self.completion_work
    }
}

impl From<Vec<ToolCallId>> for ForegroundToolRound {
    fn from(provider_order: Vec<ToolCallId>) -> Self {
        Self::new(provider_order)
    }
}

#[cfg(test)]
mod tests;
