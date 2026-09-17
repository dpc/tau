//! Read-only pricing enrichment for a canonical provider terminal.

use super::*;

/// Exact response-local cost fields and the diagnostic owed when applying them.
pub(super) struct PreparedProviderCost {
    /// Dispatch-captured rates, standalone-owner rates, or the universal
    /// fallback.
    pub(super) rates: tau_proto::EstimatedApiCostRates,
    /// Billable response increment; preparing it never changes a live ledger.
    pub(super) increment: tau_proto::EstimatedApiCost,
    /// Whether application must report the missing dispatch pricing snapshot.
    pub(super) used_fallback: bool,
}

impl Harness {
    /// Prepare pricing without consuming a rate snapshot, charging a ledger, or
    /// publishing agent stats. Absent usage stays absent, including pricing.
    pub(super) fn prepare_finished_response_estimated_cost(
        &self,
        response: &ProviderResponseFinished,
    ) -> Option<PreparedProviderCost> {
        let usage = response.usage.as_ref()?;
        let captured_rates = self
            .prompt_coordination
            .prompt_runtime
            .estimated_cost_rates
            .get(&response.agent_prompt_id)
            .copied()
            .or_else(|| {
                self.prompt_coordination
                    .standalone_accounting
                    .owners
                    .get(&response.agent_prompt_id)
                    .map(|owner| owner.estimated_cost_rates)
            });
        let rates = captured_rates.unwrap_or(tau_proto::ESTIMATED_API_COST_FALLBACK);
        Some(PreparedProviderCost {
            rates,
            increment: tau_proto::EstimatedApiCost::for_usage(usage, rates),
            used_fallback: captured_rates.is_none(),
        })
    }
}
