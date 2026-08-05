//! Extension-owned public Responses profiles and finite attempt adapter.

mod prompt_cache;
mod sampling;
#[cfg(test)]
mod tests;

pub use prompt_cache::{
    OpenAiExplicitPromptCacheMode, OpenAiPromptCache, OpenAiPromptCacheBoundary,
    OpenAiPromptCacheOptions, OpenAiPromptCachePolicy, OpenAiPromptCacheTtl,
};
use serde::de::Error as SerdeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tau_proto::{Effort, ModelName, ProviderModelInfo, ProviderName};

use self::sampling::ResponsesResponseSampler;
use crate::OpenAiPromptCacheKey;

/// One serialized generic public Responses provider profile.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesProvider {
    /// Base URL without `/responses`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// Optional bearer credential.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Explicitly configured models published under this namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ResponsesModel>,
    /// Provider-wide model tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<tau_proto::ModelTag>,
    /// Requested output-token limit, or zero to omit.
    #[serde(
        default = "default_max_output_tokens",
        skip_serializing_if = "is_default_max_output_tokens"
    )]
    pub max_output_tokens: u32,
    /// Explicit public Responses wire transport.
    #[serde(default)]
    pub transport: tau_provider_responses::Transport,
    /// Optional exact OpenAI-compatible controls shared by this route's models.
    #[serde(default, skip_serializing_if = "ResponsesCompat::is_default")]
    pub compat: ResponsesCompat,
}

/// One generic public Responses model configured by the user.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesModel {
    /// Upstream model identifier.
    pub id: ModelName,
    /// Optional supported reasoning-effort set. Omission uses every canonical
    /// effort, while an empty list disables reasoning-effort selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efforts: Option<ResponsesEfforts>,
    /// Optional model-specific public Responses wire compatibility override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<ResponsesCompat>,
    /// Optional user-facing label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Advertised input context window.
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    /// Model-specific tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<tau_proto::ModelTag>,
    /// Whether the model may issue several Function calls in one response.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub supports_parallel_tool_calls: bool,
    /// Estimated USD price per million uncached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_uncached_input_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
    /// Estimated USD price per million cached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_cached_input_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
    /// Estimated USD price per million cache-write input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_cache_write_input_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
    /// Estimated USD price per million output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_output_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
    /// Estimated USD storage price per million cache token-hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_cache_storage_cost_1m_token_hour_usd:
        Option<tau_proto::EstimatedUsdPerMillionTokenHours>,
}

/// Serialized public Responses request controls.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesCompat {
    /// Exact legacy OpenAI prompt-cache controls accepted by this route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_prompt_cache: Option<OpenAiPromptCache>,
}

impl ResponsesCompat {
    /// Return whether no public Responses compatibility control is selected.
    fn is_default(value: &Self) -> bool {
        value.openai_prompt_cache.is_none()
    }
}

/// A validated canonical set of public Responses reasoning-effort levels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesEfforts {
    /// Unique configured levels in canonical UI cycling order.
    levels: Vec<Effort>,
}

impl ResponsesEfforts {
    /// Returns the validated levels in canonical UI cycling order.
    #[must_use]
    pub fn as_slice(&self) -> &[Effort] {
        &self.levels
    }
}

impl Serialize for ResponsesEfforts {
    /// Serializes validated effort levels as the profile's direct array value.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.levels.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ResponsesEfforts {
    /// Deserializes and validates the profile's direct effort array value.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(Vec::deserialize(deserializer)?).map_err(SerdeError::custom)
    }
}

impl TryFrom<Vec<Effort>> for ResponsesEfforts {
    type Error = &'static str;

    /// Validates unique configured levels and stores them in canonical order.
    fn try_from(configured: Vec<Effort>) -> Result<Self, Self::Error> {
        if configured
            .iter()
            .enumerate()
            .any(|(index, effort)| configured[..index].contains(effort))
        {
            return Err("Responses model efforts must not contain duplicates");
        }

        Ok(Self {
            levels: Effort::ALL
                .into_iter()
                .filter(|effort| configured.contains(effort))
                .collect(),
        })
    }
}

fn model_efforts(model: &ResponsesModel) -> Vec<Effort> {
    model.efforts.as_ref().map_or_else(
        || Effort::ALL.to_vec(),
        |efforts| efforts.as_slice().to_vec(),
    )
}

fn default_context_window() -> u64 {
    128_000
}

fn default_max_output_tokens() -> u32 {
    8192
}

fn is_default_max_output_tokens(value: &u32) -> bool {
    *value == default_max_output_tokens()
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

/// Publishes configured Responses models with the text/Function-only surface.
pub fn models_for_provider(
    provider_name: &ProviderName,
    provider: &ResponsesProvider,
) -> Vec<ProviderModelInfo> {
    provider
        .models
        .iter()
        .map(|model| {
            let mut tags = provider.tags.clone();
            for tag in &model.tags {
                if !tags.contains(tag) {
                    tags.push(tag.clone());
                }
            }
            ProviderModelInfo {
                id: tau_proto::ModelId::new(provider_name.clone(), model.id.clone()),
                display_name: model.display_name.clone(),
                tags,
                supported_tool_types: vec![tau_proto::ToolType::Function],
                input_modalities: Vec::new(),
                tool_result_modalities: Vec::new(),
                supports_parallel_tool_calls: model.supports_parallel_tool_calls,
                default_affinity: 0,
                context_window: model.context_window,
                efforts: model_efforts(model),
                verbosities: vec![tau_proto::Verbosity::Medium],
                thinking_summaries: vec![tau_proto::ThinkingSummary::Off],
                supports_compaction: false,
                supports_standalone_compaction: false,
                standalone_compaction_threshold: None,
                est_uncached_input_cost_1m_usd: model.est_uncached_input_cost_1m_usd,
                est_cached_input_cost_1m_usd: model.est_cached_input_cost_1m_usd,
                est_cache_write_input_cost_1m_usd: model.est_cache_write_input_cost_1m_usd,
                est_output_cost_1m_usd: model.est_output_cost_1m_usd,
                est_cache_storage_cost_1m_token_hour_usd: model
                    .est_cache_storage_cost_1m_token_hour_usd,
            }
        })
        .collect()
}

/// Runs and samples one finite generic public Responses attempt.
#[allow(clippy::too_many_arguments)]
pub fn run_prompt_attempt<W: std::io::Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ResponsesProvider,
    model: &ResponsesModel,
    writer: &mut tau_proto::PeerOutputWriter<W>,
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
) -> PromptAttemptOutcome {
    let compat = model.compat.unwrap_or(provider.compat);
    let config = tau_provider_responses::AttemptConfig {
        base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        max_output_tokens: provider.max_output_tokens,
        transport: provider.transport,
        prompt_cache: compat.openai_prompt_cache.map(|cache| match cache.key {
            OpenAiPromptCacheKey::Agent => match cache.policy {
                OpenAiPromptCachePolicy::Legacy { retention } => {
                    tau_provider_responses::PromptCachePolicy::Legacy(match retention {
                        crate::OpenAiPromptCacheRetention::InMemory => {
                            tau_provider_responses::PromptCacheRetention::InMemory
                        }
                        crate::OpenAiPromptCacheRetention::Hours24 => {
                            tau_provider_responses::PromptCacheRetention::Hours24
                        }
                    })
                }
                OpenAiPromptCachePolicy::Explicit { .. } => {
                    tau_provider_responses::PromptCachePolicy::Explicit
                }
            },
        }),
    };
    let model = tau_provider_responses::AttemptModel {
        id: model.id.clone(),
    };
    let mut sampler = ResponsesResponseSampler::new();
    let outcome = tau_provider_responses::run_attempt(
        prompt,
        &config,
        &model,
        &mut |progress| sampler.emit_if_due(agent_prompt_id, prompt, progress, writer),
        is_canceled,
        network,
    );
    match outcome {
        tau_provider_responses::AttemptOutcome::Completed(success) => {
            sampler.latest_items = success.progress_items;
            sampler.latest_bytes = success.response_bytes_received;
            sampler.flush(agent_prompt_id, prompt, writer);
            PromptAttemptOutcome::Finished(Box::new(finished(
                agent_prompt_id,
                prompt,
                provider,
                success.output_items,
                success.stop_reason,
                None,
                None,
                success.usage,
                success.provider_response_id,
            )))
        }
        tau_provider_responses::AttemptOutcome::Retryable { decision, progress } => {
            PromptAttemptOutcome::Retry { decision, progress }
        }
        tau_provider_responses::AttemptOutcome::Canceled { progress } => {
            PromptAttemptOutcome::Canceled { progress }
        }
        tau_provider_responses::AttemptOutcome::Terminal(failure) => {
            PromptAttemptOutcome::Terminal {
                finished: Box::new(finished(
                    agent_prompt_id,
                    prompt,
                    provider,
                    Vec::new(),
                    failure.stop_reason,
                    Some(failure.message),
                    failure.failure_kind,
                    None,
                    None,
                )),
                progress: failure.progress,
            }
        }
    }
}

/// Extension-owned outcome for the generic Responses finite attempt.
pub enum PromptAttemptOutcome {
    /// Completion produced a terminal provider response.
    Finished(Box<tau_proto::ProviderResponseFinished>),
    /// Scheduler may retry from the full local transcript.
    Retry {
        decision: tau_provider::retry_policy::RetryDecision,
        progress: tau_provider_responses::AttemptProgress,
    },
    /// Cancellation won the finite attempt.
    Canceled {
        progress: tau_provider_responses::AttemptProgress,
    },
    /// A permanent failure ended the prompt.
    Terminal {
        finished: Box<tau_proto::ProviderResponseFinished>,
        progress: tau_provider_responses::AttemptProgress,
    },
}

#[allow(clippy::too_many_arguments)]
fn finished(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ResponsesProvider,
    output_items: Vec<tau_proto::ContextItem>,
    stop_reason: tau_proto::ProviderStopReason,
    error: Option<String>,
    failure_kind: Option<tau_proto::ProviderFailureKind>,
    usage: Option<tau_proto::ProviderTokenUsage>,
    provider_response_id: Option<String>,
) -> tau_proto::ProviderResponseFinished {
    tau_proto::ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        output_items,
        stop_reason,
        error,
        failure_kind,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: prompt.originator.clone(),
        usage,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: Some(tau_proto::ProviderBackend {
            kind: tau_proto::ProviderBackendKind::Responses,
            base_url: provider.base_url.clone(),
            transport: backend_transport(provider),
            stale_chain_fallback: false,
        }),
        provider_response_id,
        ws_pool_delta: None,
    }
}

fn backend_transport(provider: &ResponsesProvider) -> tau_proto::ProviderBackendTransport {
    match provider.transport {
        tau_provider_responses::Transport::Sse => tau_proto::ProviderBackendTransport::HttpSse,
        tau_provider_responses::Transport::Websocket => {
            tau_proto::ProviderBackendTransport::Websocket
        }
    }
}
