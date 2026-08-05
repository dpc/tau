//! Privacy-redacted provider cache usage observations.

use serde::{Deserialize, Serialize};

/// Provider-reported cache storage consumption in token-microseconds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CacheStorageTokenMicros(u64);

impl CacheStorageTokenMicros {
    /// Construct a token-time quantity from token-microseconds.
    #[must_use]
    pub const fn new(token_micros: u64) -> Self {
        Self(token_micros)
    }

    /// Return this quantity as token-microseconds.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Privacy-redacted provider cache observations for one response.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderCacheUsage {
    /// Tokens served from provider cache, when the route reports them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_tokens: Option<u64>,
    /// Tokens written into provider cache, when the route reports them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_tokens: Option<u64>,
    /// Tokens reported as cache misses, when the route reports them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub miss_tokens: Option<u64>,
    /// Largest cacheable prefix observed for the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cacheable_prefix_tokens: Option<u64>,
    /// Why this request could refresh provider cache residency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_reason: Option<ProviderCacheRefreshReason>,
    /// Confidence in any provider cache expiry expectation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry_confidence: Option<ProviderCacheExpiryConfidence>,
    /// Estimated prefill tokens avoided by the observed cache read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avoided_prefill_tokens: Option<u64>,
    /// Provider-reported cache storage token-time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_token_micros: Option<CacheStorageTokenMicros>,
}

impl ProviderCacheUsage {
    /// Clamp token observations deterministically to authoritative total input.
    ///
    /// Read tokens consume the available input first, followed by writes and
    /// misses. Informational prefix and avoided-prefill estimates clamp
    /// independently to total input.
    #[must_use]
    pub fn normalized(self, total_input_tokens: u64) -> Self {
        let Self {
            read_tokens,
            write_tokens,
            miss_tokens,
            cacheable_prefix_tokens,
            refresh_reason,
            expiry_confidence,
            avoided_prefill_tokens,
            storage_token_micros,
        } = self;
        let read_tokens = read_tokens.map(|tokens| tokens.min(total_input_tokens));
        let after_reads = total_input_tokens.saturating_sub(read_tokens.unwrap_or(0));
        let write_tokens = write_tokens.map(|tokens| tokens.min(after_reads));
        let after_writes = after_reads.saturating_sub(write_tokens.unwrap_or(0));
        Self {
            read_tokens,
            write_tokens,
            miss_tokens: miss_tokens.map(|tokens| tokens.min(after_writes)),
            cacheable_prefix_tokens: cacheable_prefix_tokens
                .map(|tokens| tokens.min(total_input_tokens)),
            refresh_reason,
            expiry_confidence,
            avoided_prefill_tokens: avoided_prefill_tokens
                .map(|tokens| tokens.min(total_input_tokens)),
            storage_token_micros,
        }
    }

    /// Return the cache hit ratio in millionths, or `None` without a known
    /// nonzero read-plus-miss denominator.
    #[must_use]
    pub fn hit_ratio_millionths(self) -> Option<u32> {
        let reads = self.read_tokens?;
        let misses = self.miss_tokens?;
        let denominator = reads.saturating_add(misses);
        if denominator == 0 {
            return None;
        }
        let ratio = u128::from(reads)
            .saturating_mul(1_000_000)
            .checked_div(u128::from(denominator))?;
        Some(u32::try_from(ratio).unwrap_or(1_000_000))
    }
}

/// Reason an observed request could refresh provider cache residency.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheRefreshReason {
    /// An ordinary user or agent request touched the cache.
    OrdinaryRequest,
    /// An explicit non-generating prewarm request touched the cache.
    ExplicitPrewarm,
    /// A future scheduler-originated refresh touched the cache.
    ScheduledRefresh,
}

/// Confidence category for a provider cache expiry expectation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheExpiryConfidence {
    /// The provider defines a known expiry bound.
    Known,
    /// The provider guarantees only a minimum residency.
    MinimumOnly,
    /// Residency is best-effort or probabilistic.
    Probabilistic,
    /// The route exposes cache usage without a useful expiry contract.
    Unknown,
}
