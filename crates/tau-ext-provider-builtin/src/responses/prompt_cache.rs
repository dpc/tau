//! Typed OpenAI prompt-cache profile controls for public Responses routes.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::{OpenAiPromptCacheKey, OpenAiPromptCacheRetention};

/// Exact OpenAI prompt-cache controls for one public Responses route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OpenAiPromptCache {
    /// Tau-owned namespace used to derive the stable cache key.
    pub key: OpenAiPromptCacheKey,
    /// One valid legacy or explicit cache policy.
    #[serde(flatten)]
    pub policy: OpenAiPromptCachePolicy,
}

/// One valid OpenAI prompt-cache policy for a public Responses route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum OpenAiPromptCachePolicy {
    /// Legacy automatic caching with a provider retention request.
    Legacy {
        /// Legacy OpenAI retention request.
        retention: OpenAiPromptCacheRetention,
    },
    /// Explicit caching at the first typed input-text block.
    Explicit {
        /// Exact explicit cache options supported by public Responses.
        options: OpenAiPromptCacheOptions,
    },
}

/// Typed explicit OpenAI prompt-cache options for public Responses.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiPromptCacheOptions {
    /// Explicit mode disables provider-selected implicit breakpoints.
    pub mode: OpenAiExplicitPromptCacheMode,
    /// Current OpenAI explicit-cache lifetime.
    pub ttl: OpenAiPromptCacheTtl,
    /// A boundary that preserves Tau's top-level instructions representation.
    pub boundary: OpenAiPromptCacheBoundary,
}

/// Only explicit mode is accepted for public Responses cache options.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiExplicitPromptCacheMode {
    /// Use only Tau's explicit typed input-text breakpoint.
    Explicit,
}

/// OpenAI's currently supported explicit-cache lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OpenAiPromptCacheTtl {
    /// Request OpenAI's 30-minute explicit-cache lifetime.
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

/// Permissive serialized cache shape decoded before exactly-one-policy
/// validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnvalidatedOpenAiPromptCache {
    /// Tau-owned namespace parsed before policy validation.
    key: OpenAiPromptCacheKey,
    /// Optional legacy retention parsed before exclusivity validation.
    #[serde(default)]
    retention: Option<OpenAiPromptCacheRetention>,
    /// Optional explicit options parsed before exclusivity validation.
    #[serde(default)]
    options: Option<OpenAiPromptCacheOptions>,
}

impl<'de> Deserialize<'de> for OpenAiPromptCache {
    /// Reject empty and ambiguous policy configurations.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = UnvalidatedOpenAiPromptCache::deserialize(deserializer)?;
        let policy = match (raw.retention, raw.options) {
            (Some(retention), None) => OpenAiPromptCachePolicy::Legacy { retention },
            (None, Some(options)) => OpenAiPromptCachePolicy::Explicit { options },
            (None, None) | (Some(_), Some(_)) => {
                return Err(D::Error::custom(
                    "openai_prompt_cache requires exactly one of `retention` or `options`",
                ));
            }
        };
        Ok(Self {
            key: raw.key,
            policy,
        })
    }
}
