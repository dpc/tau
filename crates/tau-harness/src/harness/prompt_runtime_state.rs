//! Owns live prompt correlation, dispatch snapshots, and replay continuations.
//!
//! Provider connection routing and compaction transaction ownership remain in
//! their dedicated runtime owners.

use super::prompt_materialization_timing::PrecheckpointMaterializationTiming;
use super::*;

/// Runtime-only state associated with provider prompts and their continuations.
#[derive(Default)]
pub(crate) struct PromptRuntimeState {
    /// Content-free pre-checkpoint timing awaiting its exact committed owner.
    pub(super) pending_materialization_timings:
        HashMap<AgentPromptId, PrecheckpointMaterializationTiming>,
    /// Owning transcript agent for every in-flight provider prompt.
    pub(super) agents: HashMap<AgentPromptId, AgentId>,
    /// Ephemeral-agent prompts retained for late provider report filtering.
    pub(super) ephemeral_provider_prompts: HashSet<AgentPromptId>,
    /// Retry correlations that targeted ephemeral agents.
    pub(super) ephemeral_provider_retry_requests: HashSet<tau_proto::RetryPromptRequestId>,
    /// Prompt identifiers already owning a live compact-fact continuation.
    pub(super) pending_dispatches: HashSet<AgentPromptId>,
    /// Provider model captured for each dispatched prompt.
    pub(super) models: HashMap<AgentPromptId, ModelId>,
    /// Cost rates captured at exact provider dispatch.
    pub(super) estimated_cost_rates: HashMap<AgentPromptId, tau_proto::EstimatedApiCostRates>,
    /// Immutable content-free context projection captured at dispatch.
    pub(super) context_limits: HashMap<AgentPromptId, PromptContextLimitSnapshot>,
    /// Effective context-size alerts captured for each prompt.
    pub(super) context_size_alerts:
        HashMap<AgentPromptId, BTreeMap<String, tau_config::settings::ContextSizeAlert>>,
    /// Automatic-compaction policies frozen with each prompt.
    pub(super) compaction_policies:
        HashMap<AgentPromptId, BTreeMap<String, tau_config::settings::CompactionPolicy>>,
    /// Proactive projection paired with each compaction policy snapshot.
    /// Prompts whose stream exposed semantic output.
    pub(super) semantic_output: HashSet<AgentPromptId>,
    /// Stale owner reports waiting for their durable closer.
    pub(super) pending_stale_provider_responses:
        HashMap<AgentPromptId, PendingStaleProviderResponse>,
    /// Restored prompt activations waiting for runtime handlers.
    pub(super) pending_replay_activation_occurrences:
        HashMap<AgentId, Vec<ReplayPromptActivationOccurrence>>,
    /// Restored uncertain owners waiting for materialized activation.
    pub(super) pending_replay_uncertain_stale: HashMap<AgentId, AgentPromptTerminated>,
    /// Harness route failures awaiting durable terminal response commit.
    pub(super) local_route_failures: HashSet<AgentPromptId>,
    /// Rejected completion-bearing steers waiting for branch reselection.
    pub(super) pending_publish_completions: HashMap<AgentId, AgentPublishCompletion>,
    /// Initial prompts awaiting their first materialized provider prompt.
    pub(super) pending_initial_correlations: HashMap<AgentId, InitialPromptCorrelation>,
    /// Provider operation and resume policy for each prompt.
    pub(super) operations: HashMap<AgentPromptId, (tau_proto::PromptOperation, bool)>,
    /// Effective tool specifications captured for each prompt.
    pub(super) tool_specs: HashMap<AgentPromptId, Vec<tau_proto::ToolSpec>>,
    /// Hidden ordinary-tool invocation policies frozen with each prompt.
    pub(super) tool_invocation_policies:
        HashMap<AgentPromptId, HashMap<ToolName, tau_proto::ToolInvocationPolicy>>,
    /// Prompt snapshot owner for each provider-emitted tool call.
    tool_call_prompts: HashMap<ToolCallId, AgentPromptId>,
    /// Exact provider-emitted tool calls retained by each prompt snapshot.
    tool_calls_by_prompt: HashMap<AgentPromptId, HashSet<ToolCallId>>,
    /// Exact call-index entries touched by prompt bookkeeping in tests.
    #[cfg(test)]
    tool_call_index_work: usize,
    /// Branch-local tool repair examples already shown to the model.
    pub(super) shown_tool_failure_examples: HashSet<(AgentId, ToolName, String)>,
}

impl PromptRuntimeState {
    /// Returns the prompt snapshot that owns one provider-emitted tool call.
    pub(crate) fn tool_call_prompt(&self, call_id: &ToolCallId) -> Option<&AgentPromptId> {
        self.tool_call_prompts.get(call_id)
    }

    /// Records one call-to-prompt backreference, defensively replacing an older
    /// owner and retiring its snapshots when that call was its last member.
    pub(super) fn record_tool_call_prompt(
        &mut self,
        call_id: ToolCallId,
        prompt_id: AgentPromptId,
    ) {
        if self.tool_call_prompts.get(&call_id) == Some(&prompt_id) {
            return;
        }
        if let Some(previous) = self
            .tool_call_prompts
            .insert(call_id.clone(), prompt_id.clone())
        {
            self.remove_prompt_backreference(&previous, &call_id);
        }
        self.tool_calls_by_prompt
            .entry(prompt_id)
            .or_default()
            .insert(call_id);
        #[cfg(test)]
        {
            self.tool_call_index_work += 1;
        }
    }

    /// Removes one actually-present call backreference and retires prompt
    /// snapshots only when no call remains for that prompt.
    pub(super) fn remove_tool_call_prompt(&mut self, call_id: &str) {
        let Some((call_id, prompt_id)) = self.tool_call_prompts.remove_entry(call_id) else {
            return;
        };
        self.remove_prompt_backreference(&prompt_id, &call_id);
    }

    /// Clears one prompt snapshot and every exact call backreference it owns.
    pub(super) fn clear_prompt_tool_snapshot(&mut self, prompt_id: &AgentPromptId) {
        self.tool_specs.remove(prompt_id);
        self.tool_invocation_policies.remove(prompt_id);
        if let Some(call_ids) = self.tool_calls_by_prompt.remove(prompt_id) {
            for call_id in call_ids {
                #[cfg(test)]
                {
                    self.tool_call_index_work += 1;
                }
                if self.tool_call_prompts.get(&call_id) == Some(prompt_id) {
                    self.tool_call_prompts.remove(&call_id);
                }
            }
        }
    }

    /// Clears every tool snapshot and call backreference at session teardown.
    pub(super) fn clear_all_tool_snapshots(&mut self) {
        self.tool_specs.clear();
        self.tool_invocation_policies.clear();
        self.tool_call_prompts.clear();
        self.tool_calls_by_prompt.clear();
    }

    /// Returns whether no tool call retains a prompt snapshot in tests.
    #[cfg(test)]
    pub(super) fn tool_call_prompts_is_empty(&self) -> bool {
        self.tool_call_prompts.is_empty()
    }

    /// Returns exact call-index work performed in tests.
    #[cfg(test)]
    pub(super) fn tool_call_index_work(&self) -> usize {
        self.tool_call_index_work
    }

    /// Removes one reverse backreference and retires an empty prompt snapshot.
    fn remove_prompt_backreference(&mut self, prompt_id: &AgentPromptId, call_id: &ToolCallId) {
        #[cfg(test)]
        {
            self.tool_call_index_work += 1;
        }
        let Some(call_ids) = self.tool_calls_by_prompt.get_mut(prompt_id) else {
            // ast-grep-ignore: debug-assert-expression-must-not-mutate
            debug_assert!(false, "forward prompt-call index lacked reverse owner");
            return;
        };
        call_ids.remove(call_id);
        let retire = call_ids.is_empty();
        if retire {
            self.tool_calls_by_prompt.remove(prompt_id);
            self.tool_specs.remove(prompt_id);
            self.tool_invocation_policies.remove(prompt_id);
        }
    }
}

#[cfg(test)]
mod tests;
