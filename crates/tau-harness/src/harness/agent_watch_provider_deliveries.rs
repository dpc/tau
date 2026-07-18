//! Bounded provider-status delivery bookkeeping for one watch subscription.
//! See `DECISION-tau-harness-watcher-provider-work`.

use std::collections::{HashMap, HashSet, VecDeque};

use tau_proto::AgentPromptId;

use super::AgentWatchProviderDeliveryKind;

/// Maximum number of nonterminal prompts retained for one subscription and
/// watched-agent turn generation.
///
/// This accommodates many serial tool-round prompts while placing a fixed bound
/// on malformed, unexpectedly concurrent, or exceptionally long status streams.
pub(crate) const MAX_TRACKED_PROVIDER_STATUS_PROMPTS: usize = 64;

/// Result of recording one provider-status delivery key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderStatusDeliveryDecision {
    /// Whether this status transition has not previously been delivered.
    pub(crate) should_deliver: bool,
    /// Whether the update predates the bucket's current turn generation.
    pub(crate) stale_generation: bool,
    /// Whether admitting the prompt evicted the oldest nonterminal prompt.
    pub(crate) capacity_evicted: bool,
    /// Whether terminal delivery retired all bookkeeping for this prompt.
    pub(crate) terminal_retired: bool,
}

/// Bounded dedupe state for one directed watch subscription.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AgentWatchProviderDeliveries {
    /// Turn generation to which all retained prompt entries belong.
    turn_generation: Option<u64>,
    /// Delivered phase/category projections grouped by provider prompt.
    prompt_deliveries: HashMap<AgentPromptId, HashSet<AgentWatchProviderDeliveryKind>>,
    /// Prompt insertion order used for deterministic capacity eviction.
    prompt_order: VecDeque<AgentPromptId>,
}

impl AgentWatchProviderDeliveries {
    /// Record a delivery projection and report whether it should be fanned out.
    ///
    /// A newer generation resets the bucket. Terminal delivery retires the
    /// complete prompt entry because no later transition for a completed
    /// provider prompt is valid. If malformed input leaves too many prompts
    /// nonterminal, the oldest prompt is evicted before admitting another.
    pub(crate) fn record(
        &mut self,
        turn_generation: u64,
        agent_prompt_id: &AgentPromptId,
        kind: AgentWatchProviderDeliveryKind,
    ) -> ProviderStatusDeliveryDecision {
        if self
            .turn_generation
            .is_some_and(|current| turn_generation < current)
        {
            return ProviderStatusDeliveryDecision {
                should_deliver: false,
                stale_generation: true,
                capacity_evicted: false,
                terminal_retired: false,
            };
        }
        if self.turn_generation != Some(turn_generation) {
            self.turn_generation = Some(turn_generation);
            self.prompt_deliveries.clear();
            self.prompt_order.clear();
        }

        let terminal = matches!(kind, AgentWatchProviderDeliveryKind::TerminalError(_));
        let mut capacity_evicted = false;
        if !terminal
            && !self.prompt_deliveries.contains_key(agent_prompt_id)
            && self.prompt_deliveries.len() == MAX_TRACKED_PROVIDER_STATUS_PROMPTS
            && let Some(oldest) = self.prompt_order.pop_front()
        {
            self.prompt_deliveries.remove(&oldest);
            capacity_evicted = true;
        }

        let inserted = self
            .prompt_deliveries
            .entry(agent_prompt_id.clone())
            .or_insert_with(|| {
                self.prompt_order.push_back(agent_prompt_id.clone());
                HashSet::new()
            })
            .insert(kind);
        if !inserted {
            return ProviderStatusDeliveryDecision {
                should_deliver: false,
                stale_generation: false,
                capacity_evicted,
                terminal_retired: false,
            };
        }

        if terminal {
            self.prompt_deliveries.remove(agent_prompt_id);
            self.prompt_order.retain(|prompt| prompt != agent_prompt_id);
        }
        ProviderStatusDeliveryDecision {
            should_deliver: true,
            stale_generation: false,
            capacity_evicted,
            terminal_retired: terminal,
        }
    }

    /// Return the number of prompt identities currently retained.
    pub(crate) fn prompt_count(&self) -> usize {
        self.prompt_deliveries.len()
    }

    /// Return the total number of retained phase/category projections.
    pub(crate) fn delivery_key_count(&self) -> usize {
        self.prompt_deliveries.values().map(HashSet::len).sum()
    }
}
