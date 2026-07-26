//! Per-agent exact accounting aggregation.

#[cfg(test)]
mod tests;

use tau_proto::AgentId;

use super::agent_summary::AgentSummary;
use super::projection_error;
use super::usage::Usage;
use crate::InspectError;

/// Mutable checked accounting totals for one agent.
#[derive(Default)]
pub(super) struct Summary {
    /// Discovered materialized prompt count.
    occurrences: u64,
    /// Durable terminal count.
    complete: u64,
    /// Valid interval count.
    elapsed_reported: u64,
    /// Sum of valid intervals.
    elapsed_sum: u64,
    /// Present sent-token sum.
    sent: Option<u64>,
    /// Present capped-cache-token sum.
    cached: Option<u64>,
    /// Present output-token sum.
    received: Option<u64>,
    /// Present estimated-cost sum.
    cost: Option<u64>,
    /// Terminal occurrences with usage.
    usage_reported: u64,
    /// Terminal occurrences with cost.
    cost_reported: u64,
}

impl Summary {
    /// Accounts one discovered prompt correlation.
    pub(super) fn add_occurrence(&mut self) -> Result<(), InspectError> {
        self.occurrences = checked_add(self.occurrences, 1, "provider prompt count")?;
        Ok(())
    }

    /// Accounts one durable terminal fact.
    pub(super) fn add_terminal(&mut self) -> Result<(), InspectError> {
        self.complete = checked_add(self.complete, 1, "complete provider prompt count")?;
        Ok(())
    }

    /// Accounts one valid recorded-at interval.
    pub(super) fn add_elapsed(&mut self, elapsed: u64) -> Result<(), InspectError> {
        self.elapsed_reported = checked_add(self.elapsed_reported, 1, "elapsed count")?;
        self.elapsed_sum = checked_add(self.elapsed_sum, elapsed, "elapsed interval sum")?;
        Ok(())
    }

    /// Accounts one present response-local usage record.
    pub(super) fn add_usage(&mut self, usage: &Usage) -> Result<(), InspectError> {
        self.usage_reported = checked_add(self.usage_reported, 1, "usage occurrence count")?;
        add_optional(&mut self.sent, usage.sent(), "input token sum")?;
        add_optional(&mut self.cached, usage.cached(), "cached token sum")?;
        add_optional(&mut self.received, usage.received(), "output token sum")
    }

    /// Accounts one present stored estimated cost.
    pub(super) fn add_cost(&mut self, picodollars: u64) -> Result<(), InspectError> {
        self.cost_reported = checked_add(self.cost_reported, 1, "cost occurrence count")?;
        add_optional(&mut self.cost, picodollars, "estimated cost sum")
    }

    /// Converts accumulated counters into one serialized agent summary.
    pub(super) fn project<'a>(&self, agent_id: &'a AgentId) -> AgentSummary<'a> {
        let cache_hit_ratio_ppm = match (self.sent, self.cached) {
            (Some(0), Some(_)) | (None, _) => None,
            (Some(sent), Some(cached)) => Some(
                u64::try_from(u128::from(cached) * 1_000_000 / u128::from(sent))
                    .expect("capped cache ratio fits u64"),
            ),
            (Some(_), None) => unreachable!("usage totals are accumulated together"),
        };
        AgentSummary {
            record_type: "agent_summary",
            agent_id,
            provider_prompt_occurrences: self.occurrences,
            provider_prompt_complete: self.complete,
            provider_prompt_incomplete: self.occurrences - self.complete,
            provider_prompt_elapsed_reported: self.elapsed_reported,
            provider_prompt_recorded_at_wall_elapsed_sum_us: self.elapsed_sum,
            prompt_sent_tokens: self.sent,
            prompt_cached_tokens: self.cached,
            response_received_tokens: self.received,
            cache_hit_ratio_ppm,
            estimated_api_cost_picodollars: self.cost,
            usage_reported_occurrences: self.usage_reported,
            usage_missing_occurrences: self.complete - self.usage_reported,
            cost_reported_occurrences: self.cost_reported,
            cost_missing_occurrences: self.complete - self.cost_reported,
        }
    }
}

fn add_optional(target: &mut Option<u64>, value: u64, label: &str) -> Result<(), InspectError> {
    *target = Some(checked_add(target.unwrap_or(0), value, label)?);
    Ok(())
}

fn checked_add(left: u64, right: u64, label: &str) -> Result<u64, InspectError> {
    left.checked_add(right)
        .ok_or_else(|| projection_error(format!("{label} exceeds u64")))
}
