//! Response-local accounting enrichment and its deferred application inputs.

use super::*;

/// Accounting prepared before terminal effects, with the original cache
/// observation retained until the cache-residency reducer runs.
pub(super) struct PreparedProviderAccounting {
    /// Original normalized usage replaced by canonical dispatch-owned usage.
    pub(super) reported_usage: Option<ProviderTokenUsage>,
    /// Whether a dispatch model and observed counters produced canonical usage.
    pub(super) usage_prepared: bool,
    /// Original ceiling needed to preserve invalid-observation diagnostics.
    pub(super) reported_cache_read_ceiling: Option<u64>,
    /// Response-local price and the deferred fallback diagnostic.
    pub(super) cost: Option<prepared_provider_cost::PreparedProviderCost>,
}

impl Harness {
    /// Enrich only the supplied response; do not consume snapshots, update
    /// ledgers, emit diagnostics, or publish stats.
    pub(super) fn prepare_provider_accounting(
        &self,
        response: &mut ProviderResponseFinished,
        update_live_totals: bool,
    ) -> PreparedProviderAccounting {
        let usage = response.usage.as_ref();
        let reported_cache_read_ceiling =
            usage.and_then(|usage| usage.prompt_cache_read_ceiling_tokens);
        let prepared = self.prepare_finished_response_usage(
            response,
            usage.map(|usage| usage.prompt_sent_tokens),
            usage.map(|usage| usage.prompt_cached_tokens),
            usage.map(|usage| usage.response_received_tokens),
            update_live_totals,
        );
        let usage_prepared = prepared.is_some();
        let reported_usage = prepared.and_then(|usage| response.usage.replace(usage));
        let cost = self.prepare_finished_response_estimated_cost(response);
        response.estimated_api_cost_rates = cost.as_ref().map(|cost| cost.rates);
        response.estimated_api_cost_increment = cost.as_ref().map(|cost| cost.increment);
        PreparedProviderAccounting {
            reported_usage,
            usage_prepared,
            reported_cache_read_ceiling,
            cost,
        }
    }
}
