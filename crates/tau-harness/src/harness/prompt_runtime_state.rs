//! Owns live prompt correlation, dispatch snapshots, and replay continuations.
//!
//! Provider connection routing and compaction transaction ownership remain in
//! their dedicated runtime owners.

use super::*;

/// Runtime-only state associated with provider prompts and their continuations.
#[derive(Default)]
pub(crate) struct PromptRuntimeState {
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
    /// Prompt snapshot owner for each provider-emitted tool call.
    pub(crate) tool_call_prompts: HashMap<ToolCallId, AgentPromptId>,
    /// Branch-local tool repair examples already shown to the model.
    pub(super) shown_tool_failure_examples: HashSet<(AgentId, ToolName, String)>,
}
