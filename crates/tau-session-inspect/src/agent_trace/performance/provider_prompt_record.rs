//! Serialized provider-prompt projection row.

use serde::Serialize;
use tau_proto::{AgentId, AgentPromptId, ModelId};

/// Serialized provider occurrence without content payloads.
#[derive(Serialize)]
pub(super) struct ProviderPromptRecord<'a> {
    /// Row discriminator.
    pub(super) record_type: &'static str,
    /// Journal owning the occurrence.
    pub(super) agent_id: &'a AgentId,
    /// Stable provider-prompt correlation ID.
    pub(super) agent_prompt_id: &'a AgentPromptId,
    /// Provider-qualified materialized model ID.
    pub(super) model: &'a ModelId,
    /// Authoritative prompt-start journal sequence.
    pub(super) journal_seq: u64,
    /// Authoritative terminal journal sequence when selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) terminal_journal_seq: Option<u64>,
    /// Relative prompt-materialization time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) at_us: Option<u64>,
    /// Relative terminal-response time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) terminal_at_us: Option<u64>,
    /// Qualified append-invocation wall interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) recorded_at_wall_elapsed_us: Option<u64>,
    /// Whether canonical terminal evidence exists.
    pub(super) terminal_present: bool,
    /// Present response-local input tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prompt_sent_tokens: Option<u64>,
    /// Present capped cached-input tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prompt_cached_tokens: Option<u64>,
    /// Present response-local output tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) response_received_tokens: Option<u64>,
    /// Present stored estimated cost increment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) estimated_api_cost_picodollars: Option<u64>,
}
