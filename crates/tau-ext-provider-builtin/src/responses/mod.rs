//! Extension-owned public Responses profiles and finite attempt adapter.

mod sampling;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use tau_proto::{ModelName, ProviderModelInfo, ProviderName};

use self::sampling::ResponsesResponseSampler;

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
}

/// One generic public Responses model configured by the user.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesModel {
    /// Upstream model identifier.
    pub id: ModelName,
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
    /// Estimated USD price per million output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_output_cost_1m_usd: Option<tau_proto::EstimatedUsdPerMillion>,
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
                efforts: vec![tau_proto::Effort::Off],
                verbosities: vec![tau_proto::Verbosity::Medium],
                thinking_summaries: vec![tau_proto::ThinkingSummary::Off],
                supports_compaction: false,
                supports_standalone_compaction: false,
                standalone_compaction_threshold: None,
                est_uncached_input_cost_1m_usd: model.est_uncached_input_cost_1m_usd,
                est_cached_input_cost_1m_usd: model.est_cached_input_cost_1m_usd,
                est_output_cost_1m_usd: model.est_output_cost_1m_usd,
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
    let config = tau_provider_responses::AttemptConfig {
        base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        max_output_tokens: provider.max_output_tokens,
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
            transport: tau_proto::ProviderBackendTransport::HttpSse,
            stale_chain_fallback: false,
        }),
        provider_response_id,
        ws_pool_delta: None,
    }
}
