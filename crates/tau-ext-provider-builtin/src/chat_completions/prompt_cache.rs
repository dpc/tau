//! Typed OpenAI prompt-cache profile controls for Chat Completions routes.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::{OpenAiPromptCacheKey, OpenAiPromptCacheRetention};

/// Exact OpenAI prompt-cache controls for one Chat Completions route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OpenAiPromptCache {
    /// Tau-owned namespace used to derive the stable cache key.
    pub key: OpenAiPromptCacheKey,
    /// One valid legacy or explicit cache policy.
    #[serde(flatten)]
    pub policy: OpenAiPromptCachePolicy,
}

impl OpenAiPromptCache {
    /// Build a legacy automatic-cache policy for the selected key namespace.
    #[must_use]
    pub const fn legacy(key: OpenAiPromptCacheKey, retention: OpenAiPromptCacheRetention) -> Self {
        Self {
            key,
            policy: OpenAiPromptCachePolicy::Legacy { retention },
        }
    }

    /// Build an explicit system-prompt-boundary policy for the selected key
    /// namespace.
    #[must_use]
    pub const fn explicit(key: OpenAiPromptCacheKey, options: OpenAiPromptCacheOptions) -> Self {
        Self {
            key,
            policy: OpenAiPromptCachePolicy::Explicit { options },
        }
    }
}

/// One valid OpenAI prompt-cache policy for a Chat Completions route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum OpenAiPromptCachePolicy {
    /// Legacy automatic caching with an explicit provider retention request.
    Legacy {
        /// Legacy OpenAI retention request.
        retention: OpenAiPromptCacheRetention,
    },
    /// Explicit caching with a typed stable-prefix boundary.
    Explicit {
        /// Explicit OpenAI cache options.
        options: OpenAiPromptCacheOptions,
    },
}

/// Typed explicit OpenAI prompt-cache options for Chat Completions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiPromptCacheOptions {
    /// Explicit mode disables the provider's implicit volatile-suffix
    /// breakpoint.
    pub mode: OpenAiExplicitPromptCacheMode,
    /// Current OpenAI minimum lifetime for explicit breakpoints.
    pub ttl: OpenAiPromptCacheTtl,
    /// Stable prompt boundary that Tau can lower without changing role
    /// semantics.
    pub boundary: OpenAiPromptCacheBoundary,
}

/// Safe prompt-cache mode supported by Tau's initial explicit-boundary
/// interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiExplicitPromptCacheMode {
    /// Use only Tau's explicit stable-prefix breakpoint.
    Explicit,
}

/// OpenAI's currently supported explicit-cache lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OpenAiPromptCacheTtl {
    /// Request the currently supported 30-minute minimum lifetime.
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

/// Raw profile shape decoded before enforcing cache-policy exclusivity.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnvalidatedOpenAiPromptCache {
    /// Tau-owned namespace parsed before building the validated policy.
    key: OpenAiPromptCacheKey,
    /// Optional legacy cache retention from the serialized profile.
    #[serde(default)]
    retention: Option<OpenAiPromptCacheRetention>,
    /// Optional explicit cache options from the serialized profile.
    #[serde(default)]
    options: Option<OpenAiPromptCacheOptions>,
}

impl<'de> Deserialize<'de> for OpenAiPromptCache {
    /// Reject ambiguous or empty cache policies while parsing a profile.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = UnvalidatedOpenAiPromptCache::deserialize(deserializer)?;
        match (raw.retention, raw.options) {
            (Some(retention), None) => Ok(Self::legacy(raw.key, retention)),
            (None, Some(options)) => Ok(Self::explicit(raw.key, options)),
            (None, None) | (Some(_), Some(_)) => Err(D::Error::custom(
                "openai_prompt_cache requires exactly one of `retention` or `options`",
            )),
        }
    }
}
