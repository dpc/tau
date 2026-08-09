//! Shared typed OpenAI prompt-cache controls for generic providers.

use serde::{Deserialize, Serialize};

/// Tau-owned prompt-cache key namespace selected for an OpenAI-compatible
/// route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiPromptCacheKey {
    /// Derive a stable key from the durable Tau agent identifier.
    Agent,
}

/// Legacy OpenAI automatic-cache retention selected for an exact route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiPromptCacheRetention {
    /// Use the provider's ordinary in-memory retention behavior.
    InMemory,
    /// Request the provider's 24-hour retention behavior.
    #[serde(rename = "24h")]
    Hours24,
}

impl OpenAiPromptCacheRetention {
    /// Return the exact legacy OpenAI wire spelling.
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::InMemory => "in_memory",
            Self::Hours24 => "24h",
        }
    }
}
