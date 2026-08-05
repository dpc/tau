use serde::de as path_serde_de;

#[cfg(test)]
mod tests;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ModelId;

/// Token usage bucket for one provider/model or the session total.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenUsageCounts {
    /// Number of requests made to the LLM backend.
    pub requests: u64,
    /// Tokens sent to the LLM backend.
    pub sent_tokens: u64,
    /// Sent tokens reported as provider-cache hits.
    pub cached_tokens: u64,
    /// Completed tokens received from the LLM backend.
    pub received_tokens: u64,
    /// Tokens received for the currently streaming response.
    pub in_progress_received_tokens: u64,
}

impl TokenUsageCounts {
    #[must_use]
    pub fn with_in_progress_received(mut self, in_progress_received_tokens: u64) -> Self {
        self.in_progress_received_tokens = in_progress_received_tokens;
        self
    }
}

/// Session-scoped token usage totals, plus per-provider/model buckets.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenUsageStats {
    pub total: TokenUsageCounts,
    pub by_model: BTreeMap<ModelId, TokenUsageCounts>,
}

impl TokenUsageStats {
    pub fn start_request(&mut self, model: &ModelId) {
        self.total.requests = self.total.requests.saturating_add(1);
        self.by_model.entry(model.clone()).or_default().requests = self
            .by_model
            .get(model)
            .map_or(1, |counts| counts.requests.saturating_add(1));
    }

    pub fn add_sent(&mut self, model: &ModelId, sent_tokens: u64, cached_tokens: u64) {
        self.total.sent_tokens = self.total.sent_tokens.saturating_add(sent_tokens);
        self.total.cached_tokens = self.total.cached_tokens.saturating_add(cached_tokens);
        let counts = self.by_model.entry(model.clone()).or_default();
        counts.sent_tokens = counts.sent_tokens.saturating_add(sent_tokens);
        counts.cached_tokens = counts.cached_tokens.saturating_add(cached_tokens);
    }

    pub fn add_received(&mut self, model: &ModelId, received_tokens: u64) {
        self.total.received_tokens = self.total.received_tokens.saturating_add(received_tokens);
        self.by_model
            .entry(model.clone())
            .or_default()
            .received_tokens = self.by_model.get(model).map_or(received_tokens, |counts| {
            counts.received_tokens.saturating_add(received_tokens)
        });
    }
}

/// Usage stats attached to one completed provider response.
///
/// `model` is `None` until the harness fills it in from the matching
/// `prompt_models` entry — providers construct `ProviderTokenUsage` without
/// knowledge of the qualified `provider/model` id.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderTokenUsage {
    /// Qualified provider/model id the harness attributes this usage to.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_model_id"
    )]
    pub model: Option<ModelId>,
    /// Input tokens sent for this response.
    pub prompt_sent_tokens: u64,
    /// Input tokens the provider reported as cache hits for this response.
    pub prompt_cached_tokens: u64,
    /// Provider assertion for the largest prompt-cache-read token count this
    /// exact completed attempt could have reported.
    ///
    /// `None` means the provider could not establish an exact ceiling.
    /// `Some(0)` proves that no reportable cache-read opportunity existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_read_ceiling_tokens: Option<u64>,
    /// Optional normalized provider-cache observations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<Box<crate::ProviderCacheUsage>>,
    /// Output tokens received for this response.
    pub response_received_tokens: u64,
    /// Session-total and per-model token counters after this response.
    pub stats: TokenUsageStats,
}

/// Deserializer for `Option<ModelId>` that maps a literal `""` to
/// `None` so persisted events written before `ModelId` got strict
/// validation (where the provider's `Default::default()` filled the
/// field with an empty string before the harness rewrote it) keep
/// replaying instead of failing a whole log decode.
fn deserialize_optional_model_id<'de, D>(deserializer: D) -> Result<Option<ModelId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(s) if s.is_empty() => Ok(None),
        Some(s) => s.parse().map(Some).map_err(path_serde_de::Error::custom),
    }
}
