//! Extension-owned Chat Completions profiles, publication, sampling, and
//! events.

use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};
use tau_proto::ModelName;
use tau_provider::local_summary_compaction::Config as SummaryCompactionConfig;

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
    /// Tool definition kinds accepted by this exact configured route.
    #[serde(
        default = "function_tool_types",
        skip_serializing_if = "is_function_tool_types"
    )]
    pub supported_tool_types: Vec<tau_proto::ToolType>,
    /// Input modalities accepted by this exact configured route.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<tau_proto::InputModality>,
    /// Modalities accepted in native Function tool-result content by this
    /// route.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_result_modalities: Vec<tau_proto::InputModality>,
    /// Whether this model may produce multiple Function calls in one turn.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub supports_parallel_tool_calls: bool,
    /// Optional full override for Tau-owned summary compaction limits.
    ///
    /// Absence derives conservative defaults from the model context window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_summary_compaction: Option<LocalSummaryCompactionConfig>,
    /// Optional operator-declared runtime cache contract for this exact model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_contract: Option<crate::ProviderCacheContract>,
    /// Estimated USD price per million uncached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_uncached_input_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
    /// Estimated USD price per million provider-reported cached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_cached_input_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
    /// Estimated USD price per million provider cache-write tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_cache_write_input_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
    /// Estimated USD price per million output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_output_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
    /// Estimated USD storage price per million provider cache token-hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_cache_storage_cost_1m_token_hour_usd:
        Option<tau_proto::EstimatedUsdPerMillionTokenHours>,
}

/// Explicit limits and compatibility profile for Tau summary compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSummaryCompactionConfig {
    /// Versioned profile identifier for local summary compaction.
    pub serialization_profile: LocalSummaryCompactionSerializationProfile,
    /// Explicit context window for this local compactor; must match the model.
    pub context_window_tokens: NonZeroU64,
    /// Input-size budget used to derive the proactive compaction threshold.
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
    pub(crate) fn validated_for(
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

/// Resolve an explicit profile or the generic no-byte-cap fallback.
fn resolved_local_summary_compaction(
    override_config: Option<LocalSummaryCompactionConfig>,
    context_window: u64,
) -> Option<SummaryCompactionConfig> {
    match override_config {
        Some(config) => config.validated_for(context_window),
        None => SummaryCompactionConfig::default_for(context_window),
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
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionsCompat {
    /// Request streamed usage.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream_options: bool,
    /// Emit `parallel_tool_calls` when tools exist.
    #[serde(default, skip_serializing_if = "is_false")]
    pub parallel_tool_calls: bool,
    /// Emit `tool_choice` when Tau must select automatic or disabled tool use.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub tool_choice: bool,
    /// Exact OpenAI prompt-cache controls accepted by this route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_prompt_cache: Option<OpenAiPromptCache>,
    /// Exact reasoning-effort capability and wire spelling for this route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ChatCompletionsReasoningEffort>,
    /// Assistant reasoning fields emitted during transcript replay.
    #[serde(
        default,
        skip_serializing_if = "ChatCompletionsReasoningReplay::is_reasoning_content"
    )]
    pub reasoning_replay: ChatCompletionsReasoningReplay,
    /// Reject historical system/developer messages after the leading system
    /// prompt.
    #[serde(default, skip_serializing_if = "is_false")]
    pub single_initial_system_message: bool,
    /// Use `max_completion_tokens`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub max_completion_tokens: bool,
    /// Explicit provider cache usage response schema, requiring streamed usage.
    #[serde(default, skip_serializing_if = "CacheUsageCompat::is_none")]
    pub cache_usage: CacheUsageCompat,
}

impl Default for ChatCompletionsCompat {
    fn default() -> Self {
        Self::without_optional_controls()
    }
}

/// Exact reasoning efforts and extended-level spelling accepted by one route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionsReasoningEffort {
    /// Effort levels published for model selection in canonical order.
    pub efforts: ChatCompletionsReasoningEfforts,
    /// Wire emission and spelling policy.
    pub wire: ChatCompletionsReasoningEffortWire,
}

/// Validated set of reasoning effort levels accepted by a Chat Completions
/// route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChatCompletionsReasoningEfforts(
    /// Bit set indexed by [`tau_proto::Effort`]; constructors and
    /// deserialization guarantee at least one bit and reject repeated
    /// configured values.
    u8,
);

/// Invalid exact reasoning-effort set supplied through the typed API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatCompletionsReasoningEffortsError {
    /// The set contains no effective effort.
    Empty,
    /// The input repeats an effort value.
    Duplicate,
}

impl std::fmt::Display for ChatCompletionsReasoningEffortsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("reasoning efforts must not be empty"),
            Self::Duplicate => formatter.write_str("reasoning efforts must not contain duplicates"),
        }
    }
}

impl std::error::Error for ChatCompletionsReasoningEffortsError {}

impl ChatCompletionsReasoningEfforts {
    /// Creates a validated non-empty set from unique effort values.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is empty or repeats a value.
    pub fn new(
        efforts: impl IntoIterator<Item = tau_proto::Effort>,
    ) -> Result<Self, ChatCompletionsReasoningEffortsError> {
        let mut mask = 0_u8;
        for effort in efforts {
            let bit = 1_u8 << effort as u8;
            if mask & bit != 0 {
                return Err(ChatCompletionsReasoningEffortsError::Duplicate);
            }
            mask |= bit;
        }
        if mask == 0 {
            return Err(ChatCompletionsReasoningEffortsError::Empty);
        }
        Ok(Self(mask))
    }

    /// Construct the historical OpenAI-compatible set through `xhigh`.
    const fn open_ai() -> Self {
        Self((1_u8 << tau_proto::Effort::Max as u8) - 1)
    }

    /// Return whether this set contains one effort level.
    #[must_use]
    pub fn contains(self, effort: tau_proto::Effort) -> bool {
        self.0 & (1 << effort as u8) != 0
    }

    /// Return the set in stable model-publication order.
    #[must_use]
    pub fn canonical(self) -> Vec<tau_proto::Effort> {
        tau_proto::Effort::ALL
            .into_iter()
            .filter(|effort| self.contains(*effort))
            .collect()
    }
}

impl Serialize for ChatCompletionsReasoningEfforts {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.canonical().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ChatCompletionsReasoningEfforts {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let configured = Vec::<tau_proto::Effort>::deserialize(deserializer)?;
        Self::new(configured).map_err(D::Error::custom)
    }
}

/// Provider-specific emission and spelling for Tau reasoning effort.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatCompletionsReasoningEffortWire {
    /// Collapse `xhigh` and `max` to OpenAI's `high`.
    OpenAi,
    /// Preserve extended spellings such as `xhigh` instead of folding to
    /// `high`.
    Literal,
    /// Publish the configured effective effort without sending a wire field.
    Omit,
}

/// Assistant reasoning fields emitted during semantic transcript replay.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatCompletionsReasoningReplay {
    /// Emit the established OpenAI-compatible `reasoning_content` field.
    #[default]
    ReasoningContent,
    /// Emit the current vLLM assistant-schema `reasoning` field.
    Reasoning,
    /// Emit both aliases for servers whose Qwen template accepts either shape.
    Both,
}

impl ChatCompletionsReasoningReplay {
    /// Returns whether serialization can omit the default replay policy.
    fn is_reasoning_content(&self) -> bool {
        *self == Self::ReasoningContent
    }
}

/// Provider-specific cache usage wire schema enabled for a compatible route.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheUsageCompat {
    /// Ignore cache-specific response fields.
    #[default]
    None,
    /// Parse OpenAI-compatible cache read/write fields.
    OpenAi,
    /// Parse DeepSeek-compatible cache hit/miss fields.
    DeepSeek,
}

impl CacheUsageCompat {
    /// Return whether no cache usage schema is enabled.
    fn is_none(value: &Self) -> bool {
        matches!(value, Self::None)
    }
}

impl Default for ChatCompletionsProvider {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            models: Vec::new(),
            tags: Vec::new(),
            max_output_tokens: default_max_output_tokens(),
            extra_body: BTreeMap::new(),
            compat: ChatCompletionsCompat::default(),
        }
    }
}

impl ChatCompletionsProvider {
    /// Reject compatibility selections that cannot produce their declared
    /// telemetry on this streaming-only adapter.
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        self.compat.validate()?;
        for (index, model) in self.models.iter().enumerate() {
            if self.models[..index]
                .iter()
                .any(|candidate| candidate.id == model.id)
            {
                return Err("Chat Completions model ids must be unique");
            }
            if !matches!(
                model.supported_tool_types.as_slice(),
                [] | [tau_proto::ToolType::Function]
            ) {
                return Err(
                    "Chat Completions supported_tool_types must be omitted, empty, or [function]",
                );
            }
            if model.supports_parallel_tool_calls && model.supported_tool_types.is_empty() {
                return Err("parallel tool calls require Function tool support");
            }
            for modalities in [&model.input_modalities, &model.tool_result_modalities] {
                if !matches!(
                    modalities.as_slice(),
                    [] | [tau_proto::InputModality::Text]
                        | [
                            tau_proto::InputModality::Text,
                            tau_proto::InputModality::Image
                        ]
                ) {
                    return Err("modalities must be omitted, [text], or [text, image]");
                }
            }
            let accepts_image_input = model
                .input_modalities
                .contains(&tau_proto::InputModality::Image);
            let accepts_image_tool_results = model
                .tool_result_modalities
                .contains(&tau_proto::InputModality::Image);
            if accepts_image_input != accepts_image_tool_results {
                return Err(
                    "Chat Completions image input and tool-result modalities must be declared together",
                );
            }
            if let Some(compat) = model.compat {
                compat.validate()?;
            }
        }
        Ok(())
    }
}

impl ChatCompletionsCompat {
    /// Compatibility defaults for routes without optional OpenAI controls.
    const fn without_optional_controls() -> Self {
        Self {
            stream_options: false,
            parallel_tool_calls: false,
            tool_choice: true,
            openai_prompt_cache: None,
            reasoning_effort: None,
            reasoning_replay: ChatCompletionsReasoningReplay::ReasoningContent,
            single_initial_system_message: false,
            max_completion_tokens: false,
            cache_usage: CacheUsageCompat::None,
        }
    }

    /// Controls used for OpenAI-compatible public endpoints.
    #[must_use]
    pub const fn openai_defaults() -> Self {
        Self {
            stream_options: true,
            parallel_tool_calls: true,
            tool_choice: true,
            openai_prompt_cache: None,
            reasoning_effort: Some(ChatCompletionsReasoningEffort {
                efforts: ChatCompletionsReasoningEfforts::open_ai(),
                wire: ChatCompletionsReasoningEffortWire::OpenAi,
            }),
            reasoning_replay: ChatCompletionsReasoningReplay::ReasoningContent,
            single_initial_system_message: false,
            max_completion_tokens: true,
            cache_usage: CacheUsageCompat::OpenAi,
        }
    }

    /// Reject internally inconsistent route compatibility.
    ///
    /// The Chat Completions adapter always streams output, but compatible
    /// servers commonly require `stream_options.include_usage` before they
    /// append usage to that stream. A selected cache schema without this
    /// capability would make telemetry depend on undocumented server defaults.
    /// Omitted effort wire fields can represent only one fixed effective
    /// server-side effort.
    fn validate(&self) -> Result<(), &'static str> {
        if self.cache_usage != CacheUsageCompat::None && !self.stream_options {
            return Err("cache_usage requires stream_options");
        }
        if self.reasoning_effort.is_some_and(|config| {
            config.wire == ChatCompletionsReasoningEffortWire::Omit
                && config.efforts.0.count_ones() != 1
        }) {
            return Err("omitted reasoning_effort wire requires exactly one effective effort");
        }
        Ok(())
    }
}

const fn default_context_window() -> u64 {
    DEFAULT_CONTEXT_WINDOW
}
fn function_tool_types() -> Vec<tau_proto::ToolType> {
    vec![tau_proto::ToolType::Function]
}
fn is_function_tool_types(tool_types: &Vec<tau_proto::ToolType>) -> bool {
    tool_types.as_slice() == [tau_proto::ToolType::Function]
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
mod prompt_cache;
mod sampling;
#[cfg(test)]
mod tests;

pub(crate) use attempt::validate_resolved_narrative_output;
pub(super) use attempt::{PromptAttemptOutcome, models_for_provider, run_prompt_attempt};
pub(super) use openrouter::fetch_openrouter_models;
pub use openrouter::{OpenRouterDiscoveryError, OpenRouterProfile};
pub use prompt_cache::{
    OpenAiExplicitPromptCacheMode, OpenAiPromptCache, OpenAiPromptCacheBoundary,
    OpenAiPromptCacheOptions, OpenAiPromptCachePolicy, OpenAiPromptCacheTtl,
};
