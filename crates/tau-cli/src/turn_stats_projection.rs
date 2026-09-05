//! Minimal retained presentation state for completed-turn statistics.

use std::time::Duration;

/// Presentation-only cache geometry used when the provider supplied no exact
/// cache-read ceiling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum CacheEstimateContext {
    /// Generic same-agent preceding-turn estimate.
    #[default]
    Generic,
    /// Empirical private ChatGPT Responses geometry for `gpt-6-astra`.
    AstraChatGptResponses,
}

/// Exact provider ceiling or the presentation context for an uncertain
/// denominator. This replaces `Option<u64>` without enlarging retained state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheReadCeilingProjection {
    /// Provider supplied an exact cache-read ceiling; the presentation context
    /// remains available to classify the next adjacent response.
    Exact {
        /// Exact provider ceiling.
        ceiling: u64,
        /// Route/model context retained independently of exactness.
        context: CacheEstimateContext,
    },
    /// Provider supplied no exact ceiling; use this presentation context.
    Estimated(CacheEstimateContext),
}

/// Scalar usage values needed to present one completed provider response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TurnStatsUsageProjection {
    /// Input tokens sent for this response.
    pub(crate) prompt_sent_tokens: u64,
    /// Input tokens served from the provider cache.
    pub(crate) prompt_cached_tokens: u64,
    /// Exact provider ceiling or context for an uncertain UI estimate.
    pub(crate) cache_read_ceiling: CacheReadCeilingProjection,
    /// Output tokens received for this response.
    pub(crate) response_received_tokens: u64,
}

impl From<&tau_proto::ProviderTokenUsage> for TurnStatsUsageProjection {
    fn from(usage: &tau_proto::ProviderTokenUsage) -> Self {
        Self {
            prompt_sent_tokens: usage.prompt_sent_tokens,
            prompt_cached_tokens: usage.prompt_cached_tokens,
            cache_read_ceiling: usage.prompt_cache_read_ceiling_tokens.map_or(
                CacheReadCeilingProjection::Estimated(CacheEstimateContext::Generic),
                |ceiling| CacheReadCeilingProjection::Exact {
                    ceiling,
                    context: CacheEstimateContext::Generic,
                },
            ),
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
    /// Route/model context that qualified the preceding estimate.
    pub(crate) cache_estimate_context: CacheEstimateContext,
}

impl From<(TurnStatsUsageProjection, CacheEstimateContext)> for PreviousTurnUsageProjection {
    fn from(
        (usage, cache_estimate_context): (TurnStatsUsageProjection, CacheEstimateContext),
    ) -> Self {
        Self {
            prompt_sent_tokens: usage.prompt_sent_tokens,
            response_received_tokens: usage.response_received_tokens,
            cache_estimate_context,
        }
    }
}

impl TurnStatsUsageProjection {
    /// Retains route/model context independently of whether the provider
    /// supplied an exact ceiling.
    pub(crate) fn with_estimate_context(mut self, context: CacheEstimateContext) -> Self {
        self.cache_read_ceiling = match self.cache_read_ceiling {
            CacheReadCeilingProjection::Exact { ceiling, .. } => {
                CacheReadCeilingProjection::Exact { ceiling, context }
            }
            CacheReadCeilingProjection::Estimated(_) => {
                CacheReadCeilingProjection::Estimated(context)
            }
        };
        self
    }

    /// Returns the uncertain-estimate context retained for a later turn.
    pub(crate) fn estimate_context(self) -> CacheEstimateContext {
        match self.cache_read_ceiling {
            CacheReadCeilingProjection::Exact { context, .. }
            | CacheReadCeilingProjection::Estimated(context) => context,
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
