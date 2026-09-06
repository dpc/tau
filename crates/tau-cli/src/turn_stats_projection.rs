//! Minimal retained presentation state for completed-turn statistics.

use std::num::NonZeroU16;
use std::time::Duration;

/// Private ChatGPT model family eligible for passive UI calibration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheEstimateModel {
    /// Private Sol model.
    Sol,
    /// Private Terra model.
    Terra,
    /// Private Luna model.
    Luna,
    /// Private Astra model.
    Astra,
}

/// Typed request scope used to qualify a provisional cache estimate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CacheEstimateContext {
    /// Packed exact private model/control identity, or none for generic
    /// fallback.
    key: Option<NonZeroU16>,
}

impl CacheEstimateContext {
    /// Creates one exact private-route scope from observed model and controls.
    pub(crate) fn private(model: CacheEstimateModel, model_params: tau_proto::ModelParams) -> Self {
        use tau_proto::{EffectiveReasoningEffort, ServiceTier};

        let model = match model {
            CacheEstimateModel::Sol => 1,
            CacheEstimateModel::Terra => 2,
            CacheEstimateModel::Luna => 3,
            CacheEstimateModel::Astra => 4,
        };
        // Only the frozen effective effort affects the provider request. The
        // portable requested intent may differ while lowering to the same wire
        // control, so it must not split an otherwise identical cache scope.
        let (effective_kind, effective_level) = match model_params.effort.effective {
            EffectiveReasoningEffort::ProviderDefault(None) => (0, 0),
            EffectiveReasoningEffort::ProviderDefault(Some(level)) => (1, level as u16),
            EffectiveReasoningEffort::Native(level) => (2, level as u16),
            EffectiveReasoningEffort::Fixed(level) => (3, level as u16),
            EffectiveReasoningEffort::Unsupported => (4, 0),
        };
        let service_tier = match model_params.service_tier {
            None => 0,
            Some(ServiceTier::Fast) => 1,
            Some(ServiceTier::Flex) => 2,
        };
        let packed = model
            | effective_kind << 3
            | effective_level << 6
            | u16::from(model_params.verbosity.as_u8()) << 9
            | u16::from(model_params.thinking_summary.as_u8()) << 11
            | service_tier << 13;
        Self {
            key: NonZeroU16::new(packed),
        }
    }

    /// Returns whether both contexts identify the same qualified request scope.
    pub(crate) fn continues(self, previous: Self) -> bool {
        self.key.is_some() && self == previous
    }
}

/// Empirical reported-read geometry learned from an earlier qualified turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheEstimateGeometry {
    /// Recent 128-token boundary with a 182-token effective lag.
    Step128Lag182,
    /// 1,024-token boundary whose reported counts are 256 modulo 1,024.
    Step1024Residue256,
    /// Earlier 1,024-token boundary whose reported counts are 512 modulo 1,024.
    Step1024Residue512,
}

/// Confidence retained for one passively observed cache geometry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum CacheEstimateCalibration {
    /// No usable observation is retained.
    #[default]
    Uncalibrated,
    /// One exact observation; another matching turn is required before use.
    Candidate(CacheEstimateGeometry),
    /// At least two consecutive exact observations support this regime.
    Confirmed(CacheEstimateGeometry),
}

impl CacheEstimateGeometry {
    /// Smallest predecessor input covered by the passive calibration evidence.
    const MIN_OBSERVED_PREFIX_TOKENS: u64 = 10_000;

    /// Estimates the reported-read envelope for one observed predecessor input.
    pub(crate) fn estimate(self, predecessor_input: u64) -> Option<u64> {
        if predecessor_input < Self::MIN_OBSERVED_PREFIX_TOKENS {
            return None;
        }
        match self {
            Self::Step128Lag182 => Some(
                predecessor_input
                    .saturating_sub(182)
                    .div_euclid(128)
                    .saturating_mul(128),
            ),
            Self::Step1024Residue256 => Some(
                predecessor_input
                    .saturating_sub(438)
                    .div_euclid(1_024)
                    .saturating_mul(1_024)
                    .saturating_add(256),
            ),
            Self::Step1024Residue512 => Some(
                predecessor_input
                    .saturating_sub(591)
                    .div_euclid(1_024)
                    .saturating_mul(1_024)
                    .saturating_add(512),
            ),
        }
    }

    /// Learns a unique known regime from one qualified predecessor/read pair.
    pub(crate) fn infer(predecessor_input: u64, cached_input: u64) -> Option<Self> {
        if cached_input == 0 {
            return None;
        }
        let mut matched = [
            Self::Step128Lag182,
            Self::Step1024Residue256,
            Self::Step1024Residue512,
        ]
        .into_iter()
        .filter(|geometry| geometry.estimate(predecessor_input) == Some(cached_input));
        let geometry = matched.next()?;
        matched.next().is_none().then_some(geometry)
    }
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
                CacheReadCeilingProjection::Estimated(CacheEstimateContext::default()),
                |ceiling| CacheReadCeilingProjection::Exact {
                    ceiling,
                    context: CacheEstimateContext::default(),
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
    /// Regime confidence learned from preceding qualified responses.
    pub(crate) cache_estimate_calibration: CacheEstimateCalibration,
}

impl From<(TurnStatsUsageProjection, CacheEstimateContext)> for PreviousTurnUsageProjection {
    fn from(
        (usage, cache_estimate_context): (TurnStatsUsageProjection, CacheEstimateContext),
    ) -> Self {
        Self {
            prompt_sent_tokens: usage.prompt_sent_tokens,
            response_received_tokens: usage.response_received_tokens,
            cache_estimate_context,
            cache_estimate_calibration: CacheEstimateCalibration::Uncalibrated,
        }
    }
}

impl PreviousTurnUsageProjection {
    /// Retains one completed turn and passively calibrates its qualified
    /// regime.
    pub(crate) fn from_completed(usage: TurnStatsUsageProjection, previous: Option<Self>) -> Self {
        let cache_estimate_context = usage.estimate_context();
        let cache_estimate_calibration = previous
            .filter(|previous| cache_estimate_context.continues(previous.cache_estimate_context))
            .map_or(CacheEstimateCalibration::Uncalibrated, |previous| {
                let observed = CacheEstimateGeometry::infer(
                    previous.prompt_sent_tokens,
                    usage.prompt_cached_tokens,
                );
                match (previous.cache_estimate_calibration, observed) {
                    (
                        CacheEstimateCalibration::Candidate(expected)
                        | CacheEstimateCalibration::Confirmed(expected),
                        Some(observed),
                    ) if expected == observed => CacheEstimateCalibration::Confirmed(observed),
                    (_, Some(observed)) => CacheEstimateCalibration::Candidate(observed),
                    _ => CacheEstimateCalibration::Uncalibrated,
                }
            });
        Self {
            prompt_sent_tokens: usage.prompt_sent_tokens,
            response_received_tokens: usage.response_received_tokens,
            cache_estimate_context,
            cache_estimate_calibration,
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
