//! Typed OpenAI prompt-cache profile controls for public Responses routes.

use serde::de::{Error as _, IgnoredAny};
use serde::{Deserialize, Deserializer, Serialize};

use crate::OpenAiPromptCacheKey;

/// Exact OpenAI prompt-cache controls for one public Responses route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OpenAiPromptCache {
    /// Tau-owned namespace used to derive the stable cache key.
    pub key: OpenAiPromptCacheKey,
    /// Independent cache mode, lifetime, and boundary controls.
    pub options: OpenAiPromptCacheOptions,
}

/// Typed OpenAI prompt-cache options for public Responses.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiPromptCacheOptions {
    /// Select whether OpenAI or Tau chooses the cache breakpoint.
    pub mode: OpenAiPromptCacheMode,
    /// Current OpenAI cache lifetime.
    pub ttl: OpenAiPromptCacheTtl,
    /// Required Tau-owned breakpoint for explicit mode and absent for implicit
    /// mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<OpenAiPromptCacheBoundary>,
}

/// Public Responses cache breakpoint selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiPromptCacheMode {
    /// Let OpenAI select an automatic breakpoint without a Tau content marker.
    Implicit,
    /// Mark Tau's typed input-text breakpoint and disable implicit selection.
    Explicit,
}

/// OpenAI's currently supported public cache lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OpenAiPromptCacheTtl {
    /// Request OpenAI's 30-minute public cache lifetime.
    #[serde(rename = "30m")]
    Minutes30,
}

/// Explicit Responses boundary which leaves top-level instructions unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiPromptCacheBoundary {
    /// Mark the earliest Tau-constructed non-assistant input-text block.
    FirstInputText,
}

/// Permissive serialized cache shape decoded before migration validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnvalidatedOpenAiPromptCache {
    /// Tau-owned namespace parsed before cache-option validation.
    key: OpenAiPromptCacheKey,
    /// Retired public request control parsed only to report migration guidance.
    #[serde(default)]
    retention: RetiredRetention,
    /// Required independent cache options.
    #[serde(default)]
    options: Option<OpenAiPromptCacheOptions>,
}

impl<'de> Deserialize<'de> for OpenAiPromptCache {
    /// Reject retired retention and incomplete cache controls.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = UnvalidatedOpenAiPromptCache::deserialize(deserializer)?;
        if matches!(raw.retention, RetiredRetention::Present) {
            return Err(D::Error::custom(
                "openai_prompt_cache.retention is retired; use `options` with `mode` and `ttl` instead; \
                 legacy prompt_cache_retention `24h` is not equivalent to the new `30m` TTL",
            ));
        }
        let options = raw.options.ok_or_else(|| {
            D::Error::custom("openai_prompt_cache requires `options` with `mode` and `ttl`")
        })?;
        Ok(Self {
            key: raw.key,
            options,
        })
    }
}

/// Tracks whether a retired `retention` member was present, including `null`.
#[derive(Default)]
enum RetiredRetention {
    /// The profile omitted the retired member.
    #[default]
    Absent,
    /// The profile supplied the retired member with any value.
    Present,
}

impl<'de> Deserialize<'de> for RetiredRetention {
    /// Consume a retired member solely to produce deterministic migration
    /// advice.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let _ = IgnoredAny::deserialize(deserializer)?;
        Ok(Self::Present)
    }
}

impl<'de> Deserialize<'de> for OpenAiPromptCacheOptions {
    /// Require a Tau boundary exactly when explicit mode selects one.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct UnvalidatedOpenAiPromptCacheOptions {
            /// Requested automatic or explicit breakpoint selection.
            mode: OpenAiPromptCacheMode,
            /// Requested cache lifetime.
            ttl: OpenAiPromptCacheTtl,
            /// Optional boundary validated against the requested mode.
            #[serde(default)]
            boundary: Option<OpenAiPromptCacheBoundary>,
        }

        let raw = UnvalidatedOpenAiPromptCacheOptions::deserialize(deserializer)?;
        match (raw.mode, raw.boundary) {
            (OpenAiPromptCacheMode::Implicit, None)
            | (OpenAiPromptCacheMode::Explicit, Some(_)) => Ok(Self {
                mode: raw.mode,
                ttl: raw.ttl,
                boundary: raw.boundary,
            }),
            (OpenAiPromptCacheMode::Implicit, Some(_)) => Err(D::Error::custom(
                "openai_prompt_cache.options.boundary requires `mode: explicit`",
            )),
            (OpenAiPromptCacheMode::Explicit, None) => Err(D::Error::custom(
                "openai_prompt_cache.options.mode `explicit` requires `boundary`",
            )),
        }
    }
}
