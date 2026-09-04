//! Typed OpenAI prompt-cache profile controls for Chat Completions routes.

use serde::de::{Error as _, IgnoredAny};
use serde::{Deserialize, Deserializer, Serialize};

use crate::OpenAiPromptCacheKey;

/// Exact OpenAI prompt-cache controls for one Chat Completions route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OpenAiPromptCache {
    /// Tau-owned namespace used to derive the stable cache key.
    pub key: OpenAiPromptCacheKey,
    /// Independent cache mode, lifetime, and boundary controls.
    pub options: OpenAiPromptCacheOptions,
}

/// Typed OpenAI prompt-cache options for Chat Completions.
///
/// The representation makes implicit and explicit selection disjoint: implicit
/// options cannot carry a Tau boundary, and explicit options always carry one.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum OpenAiPromptCacheOptions {
    /// Let OpenAI select an automatic breakpoint without a Tau content marker.
    Implicit {
        /// Current OpenAI cache lifetime.
        ttl: OpenAiPromptCacheTtl,
    },
    /// Mark Tau's stable system-prompt boundary and disable implicit selection.
    Explicit {
        /// Current OpenAI cache lifetime.
        ttl: OpenAiPromptCacheTtl,
        /// Tau-owned breakpoint required for explicit selection.
        boundary: OpenAiPromptCacheBoundary,
    },
}

/// Public Chat Completions cache breakpoint selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiPromptCacheMode {
    /// Let OpenAI select an automatic breakpoint without a Tau content marker.
    Implicit,
    /// Mark Tau's stable system-prompt boundary and disable implicit selection.
    Explicit,
}

/// OpenAI's currently supported public cache lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OpenAiPromptCacheTtl {
    /// Request the currently supported 30-minute public cache lifetime.
    #[serde(rename = "30m")]
    Minutes30,
}

/// Stable prompt boundary supported by the Chat Completions lowering.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiPromptCacheBoundary {
    /// Mark the end of the system prompt and leave the transcript suffix
    /// unmarked.
    SystemPrompt,
}

/// Raw profile shape decoded before migration validation.
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
            (OpenAiPromptCacheMode::Implicit, None) => Ok(Self::Implicit { ttl: raw.ttl }),
            (OpenAiPromptCacheMode::Explicit, Some(boundary)) => Ok(Self::Explicit {
                ttl: raw.ttl,
                boundary,
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
