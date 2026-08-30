//! Minimal retained presentation state for completed-turn statistics.

use std::time::Duration;

/// Scalar usage values needed to present one completed provider response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TurnStatsUsageProjection {
    /// Input tokens sent for this response.
    pub(crate) prompt_sent_tokens: u64,
    /// Input tokens served from the provider cache.
    pub(crate) prompt_cached_tokens: u64,
    /// Exact cache-read ceiling, when the provider supplied one.
    pub(crate) prompt_cache_read_ceiling_tokens: Option<u64>,
    /// Output tokens received for this response.
    pub(crate) response_received_tokens: u64,
}

impl From<&tau_proto::ProviderTokenUsage> for TurnStatsUsageProjection {
    fn from(usage: &tau_proto::ProviderTokenUsage) -> Self {
        Self {
            prompt_sent_tokens: usage.prompt_sent_tokens,
            prompt_cached_tokens: usage.prompt_cached_tokens,
            prompt_cache_read_ceiling_tokens: usage.prompt_cache_read_ceiling_tokens,
            response_received_tokens: usage.response_received_tokens,
        }
    }
}

/// Previous-turn values needed to estimate the reusable prompt prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreviousTurnUsageProjection {
    /// Input tokens sent for the preceding response.
    pub(crate) prompt_sent_tokens: u64,
    /// Output tokens received for the preceding response.
    pub(crate) response_received_tokens: u64,
}

impl From<TurnStatsUsageProjection> for PreviousTurnUsageProjection {
    fn from(usage: TurnStatsUsageProjection) -> Self {
        Self {
            prompt_sent_tokens: usage.prompt_sent_tokens,
            response_received_tokens: usage.response_received_tokens,
        }
    }
}

/// Per-agent cumulative values shown in a completed-turn statistics block.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CumulativeTurnUsageProjection {
    /// Input tokens sent across completed responses.
    pub(crate) sent_tokens: u64,
    /// Input tokens served from cache across completed responses.
    pub(crate) cached_tokens: u64,
    /// Output tokens received across completed responses.
    pub(crate) received_tokens: u64,
}

impl From<tau_proto::TokenUsageCounts> for CumulativeTurnUsageProjection {
    fn from(usage: tau_proto::TokenUsageCounts) -> Self {
        Self {
            sent_tokens: usage.sent_tokens,
            cached_tokens: usage.cached_tokens,
            received_tokens: usage.received_tokens,
        }
    }
}

/// Complete allocation-free projection retained for one turn-stat block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TurnStatsPresentationProjection {
    /// Current response usage displayed by the block.
    pub(crate) usage: TurnStatsUsageProjection,
    /// Owning agent's cumulative usage after the response.
    pub(crate) cumulative_usage: CumulativeTurnUsageProjection,
    /// Same-agent preceding response usage, if one exists.
    pub(crate) previous_usage: Option<PreviousTurnUsageProjection>,
    /// Latency of the current response.
    pub(crate) turn_latency: Option<Duration>,
    /// Owning agent's cumulative response latency.
    pub(crate) total_latency: Option<Duration>,
}
