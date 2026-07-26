//! Serialized per-agent performance summary row.

use serde::Serialize;
use tau_proto::AgentId;

/// Serialized exact accounting and qualified interval totals for one agent.
#[derive(Serialize)]
pub(super) struct AgentSummary<'a> {
    /// Row discriminator.
    pub(super) record_type: &'static str,
    /// Journal summarized by this row.
    pub(super) agent_id: &'a AgentId,
    /// Number of materialized ordinary-inference prompts.
    pub(super) provider_prompt_occurrences: u64,
    /// Occurrences with canonical terminal evidence.
    pub(super) provider_prompt_complete: u64,
    /// Occurrences lacking canonical terminal evidence.
    pub(super) provider_prompt_incomplete: u64,
    /// Occurrences with a qualified wall interval.
    pub(super) provider_prompt_elapsed_reported: u64,
    /// Sum of qualified, possibly overlapping wall intervals.
    pub(super) provider_prompt_recorded_at_wall_elapsed_sum_us: u64,
    /// Sum over occurrences carrying input-token evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prompt_sent_tokens: Option<u64>,
    /// Sum over capped cached-input evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prompt_cached_tokens: Option<u64>,
    /// Sum over occurrences carrying output-token evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) response_received_tokens: Option<u64>,
    /// Integer cache ratio in parts per million.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cache_hit_ratio_ppm: Option<u64>,
    /// Sum of stored estimated cost increments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) estimated_api_cost_picodollars: Option<u64>,
    /// Terminal occurrences carrying usage.
    pub(super) usage_reported_occurrences: u64,
    /// Terminal occurrences lacking usage.
    pub(super) usage_missing_occurrences: u64,
    /// Terminal occurrences carrying calculated cost.
    pub(super) cost_reported_occurrences: u64,
    /// Terminal occurrences lacking calculated cost.
    pub(super) cost_missing_occurrences: u64,
}
