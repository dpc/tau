//! Extension-owned Chat Completions profiles, publication, sampling, and
//! events.

use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};
use tau_proto::ModelName;

/// Default context window advertised by configured compatible models.
const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// One serialized Chat Completions-compatible provider profile.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionsProvider {
    /// Base URL without `/chat/completions`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// Optional bearer token.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Harness-authorized logical secret name supplying the bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_secret: Option<String>,
    /// Models published under the profile namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ChatCompletionsModel>,
    /// Provider-wide model tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<tau_proto::ModelTag>,
    /// Requested output-token limit, or zero to omit.
    #[serde(
        default = "default_max_output_tokens",
        skip_serializing_if = "is_default_max_output_tokens"
    )]
    pub max_output_tokens: u32,
    /// Non-standard, non-conflicting request members.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_body: BTreeMap<String, serde_json::Value>,
    /// Optional OpenAI-compatible request controls.
    #[serde(default)]
    pub compat: ChatCompletionsCompat,
}

/// One configured compatible model.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionsModel {
    /// Upstream wire model id.
    pub id: ModelName,
    /// Optional display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Published context window.
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    /// Optional model-level wire compatibility override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<ChatCompletionsCompat>,
    /// Model-specific tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<tau_proto::ModelTag>,
    /// Whether this model may produce multiple Function calls in one turn.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub supports_parallel_tool_calls: bool,
    /// Optional Tau-owned summary compactor configuration.
    ///
    /// Absence keeps standalone compaction unsupported for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_summary_compaction: Option<LocalSummaryCompactionConfig>,
    /// Estimated USD price per million uncached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_uncached_input_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
    /// Estimated USD price per million provider-reported cached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_cached_input_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
    /// Estimated USD price per million output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_output_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
}

/// Explicit limits and serialization profile for Tau summary compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSummaryCompactionConfig {
    /// Versioned canonical transcript serialization accepted by the model.
    pub serialization_profile: LocalSummaryCompactionSerializationProfile,
    /// Explicit context window for this local compactor; must match the model.
    pub context_window_tokens: NonZeroU64,
    /// Maximum serialized compactor input size in bytes.
    pub max_input_bytes: NonZeroU64,
    /// Maximum requested summary output tokens.
    pub max_output_tokens: NonZeroU32,
    /// Maximum accepted summary output size in bytes.
    pub max_output_bytes: NonZeroU64,
}

/// Canonical transcript serialization supported by Tau's summary compactor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalSummaryCompactionSerializationProfile {
    /// Tau's canonical JSON transcript serialization version 1.
    LocalTranscriptV1,
}

impl LocalSummaryCompactionConfig {
    /// Convert this serialized profile into validated provider request limits.
    fn validated_for(
        self,
        model_context_window: u64,
    ) -> Option<tau_provider_chat_completions::LocalSummaryCompactionConfig> {
        tau_provider_chat_completions::LocalSummaryCompactionConfig::new(
            self.context_window_tokens,
            model_context_window,
            self.max_input_bytes,
            self.max_output_tokens,
            self.max_output_bytes,
        )
    }
}

/// Built-in default estimated prices for compatible models whose profile omits
/// explicit pricing, keyed by wire model id.
///
/// This mirrors how the ChatGPT/Codex provider special-cases its own model
/// names: a known id publishes provider pricing even without profile fields,
/// while unknown ids keep the central GPT-5.6-equivalent fallback. Explicit
/// profile `est_*` fields always take precedence over this table.
fn builtin_estimated_prices(
    model: &ModelName,
) -> Option<(
    tau_proto::EstimatedUsdPerMillion,
    tau_proto::EstimatedUsdPerMillion,
    tau_proto::EstimatedUsdPerMillion,
)> {
    let (uncached, cached, output) = match model.as_str() {
        // DeepSeek standard API prices from
        // <https://api-docs.deepseek.com/quick_start/pricing>: $0.14 uncached
        // input, $0.0028 cached input, $0.28 output per million tokens.
        "deepseek-v4-flash" => (140_000, 2_800, 280_000),
        _ => return None,
    };
    Some((
        tau_proto::EstimatedUsdPerMillion::from_micro_usd(uncached),
        tau_proto::EstimatedUsdPerMillion::from_micro_usd(cached),
        tau_proto::EstimatedUsdPerMillion::from_micro_usd(output),
    ))
}

/// Serialized OpenAI-compatible request controls.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionsCompat {
    /// Request streamed usage.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream_options: bool,
    /// Emit `parallel_tool_calls` when tools exist.
    #[serde(default, skip_serializing_if = "is_false")]
    pub parallel_tool_calls: bool,
    /// Emit a stable prompt cache key.
    #[serde(default, skip_serializing_if = "is_false")]
    pub prompt_cache_key: bool,
    /// Emit reasoning effort.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reasoning_effort: bool,
    /// Use `max_completion_tokens`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub max_completion_tokens: bool,
}

impl Default for ChatCompletionsProvider {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            api_key_secret: None,
            models: Vec::new(),
            tags: Vec::new(),
            max_output_tokens: default_max_output_tokens(),
            extra_body: BTreeMap::new(),
            compat: ChatCompletionsCompat::default(),
        }
    }
}

impl ChatCompletionsCompat {
    /// Controls used for OpenAI-compatible public endpoints.
    #[must_use]
    pub const fn openai_defaults() -> Self {
        Self {
            stream_options: true,
            parallel_tool_calls: true,
            prompt_cache_key: true,
            reasoning_effort: true,
            max_completion_tokens: true,
        }
    }
}

const fn default_context_window() -> u64 {
    DEFAULT_CONTEXT_WINDOW
}
const fn default_max_output_tokens() -> u32 {
    tau_provider_chat_completions::DEFAULT_MAX_OUTPUT_TOKENS
}
const fn default_true() -> bool {
    true
}
fn is_false(value: &bool) -> bool {
    !*value
}
fn is_true(value: &bool) -> bool {
    *value
}
fn is_default_max_output_tokens(value: &u32) -> bool {
    *value == default_max_output_tokens()
}

mod attempt;
mod openrouter;
mod sampling;
#[cfg(test)]
mod tests;

pub(super) use attempt::{PromptAttemptOutcome, models_for_provider, run_prompt_attempt};
pub(super) use openrouter::fetch_openrouter_models;
pub use openrouter::{OpenRouterDiscoveryError, OpenRouterProfile};
