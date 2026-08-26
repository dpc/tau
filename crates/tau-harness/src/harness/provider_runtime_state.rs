//! Owns provider declarations, cache refresh, quota epochs, and live dispatches.
//!
//! Prompt transcript and response-policy ownership remain in prompt runtime
//! state; this owner tracks only the provider route selected for live work.

use super::*;

/// Runtime state tied to provider declarations and connection ownership.
pub(crate) struct ProviderRuntimeState {
    /// All currently available model identifiers.
    pub(super) available_models: Vec<ModelId>,
    /// Model snapshots keyed by publishing provider connection.
    pub(super) models_by_extension: HashMap<tau_proto::ConnectionId, Vec<ProviderModelInfo>>,
    /// Flattened provider metadata keyed by model identifier.
    pub(super) model_info: HashMap<ModelId, ProviderModelInfo>,
    /// Selected provider connection for each model identifier.
    pub(super) model_routes: HashMap<ModelId, tau_proto::ConnectionId>,
    /// Single process-only owner of bounded provider cache refresh work.
    pub(super) cache_residency: ProviderCacheResidency<RuntimeCacheClock, RuntimeCacheJitter>,
    /// Foreground cohort owning the current finite cache-refresh window.
    pub(super) cache_refresh_tool_window_calls: HashSet<ToolCallId>,
    /// Validated account-quota snapshots keyed by provider namespace.
    pub(super) quota: HashMap<tau_proto::ProviderName, CurrentProviderQuota>,
    /// Empty latest snapshots retaining provider quota capability.
    pub(super) quota_capabilities:
        HashMap<tau_proto::ProviderName, tau_proto::HarnessProviderQuotaChanged>,
    /// Last cleared upstream positions for authoritative recovery.
    pub(super) quota_tombstones: HashMap<tau_proto::ProviderName, ProviderQuotaTombstone>,
    /// Bounded epochs rejected after clear or replacement.
    pub(super) quota_retired_epochs:
        HashMap<tau_proto::ProviderName, VecDeque<tau_proto::ProviderQuotaEpoch>>,
    /// Provider connection owning each in-flight prompt request.
    pub(super) pending_prompts: HashMap<AgentPromptId, tau_proto::ConnectionId>,
}

impl ProviderRuntimeState {
    /// Creates empty provider runtime state with the configured cache policy.
    pub(crate) fn new(refresh: tau_config::settings::ProviderCacheRefresh) -> Self {
        Self {
            available_models: Vec::new(),
            models_by_extension: HashMap::new(),
            model_info: HashMap::new(),
            model_routes: HashMap::new(),
            cache_residency: ProviderCacheResidency::runtime(refresh),
            cache_refresh_tool_window_calls: HashSet::new(),
            quota: HashMap::new(),
            quota_capabilities: HashMap::new(),
            quota_tombstones: HashMap::new(),
            quota_retired_epochs: HashMap::new(),
            pending_prompts: HashMap::new(),
        }
    }
}
