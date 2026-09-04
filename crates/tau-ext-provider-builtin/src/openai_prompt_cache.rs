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
