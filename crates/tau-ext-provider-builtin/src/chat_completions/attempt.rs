//! Extension policy adapter for one finite Chat Completions attempt.

use super::sampling::ResponseSampler;
use super::{ChatCompletionsModel, ChatCompletionsProvider};

/// Return publication records for one extension-owned compatible profile.
pub fn models_for_provider(
    provider_name: &tau_proto::ProviderName,
    provider: &ChatCompletionsProvider,
) -> Vec<tau_proto::ProviderModelInfo> {
    provider
        .models
        .iter()
        .map(|model| {
            let mut tags = provider.tags.clone();
            model.tags.iter().for_each(|tag| {
                if !tags.contains(tag) {
                    tags.push(tag.clone());
                }
            });
            let compat = model.compat.unwrap_or(provider.compat);
            tau_proto::ProviderModelInfo {
                id: tau_proto::ModelId::new(provider_name.clone(), model.id.clone()),
                display_name: model.display_name.clone(),
                tags,
                supported_tool_types: vec![tau_proto::ToolType::Function],
                input_modalities: Vec::new(),
                tool_result_modalities: Vec::new(),
                supports_parallel_tool_calls: model.supports_parallel_tool_calls,
                default_affinity: 0,
                context_window: model.context_window,
                efforts: if compat.reasoning_effort {
                    vec![
                        tau_proto::Effort::Off,
                        tau_proto::Effort::Minimal,
                        tau_proto::Effort::Low,
                        tau_proto::Effort::Medium,
                        tau_proto::Effort::High,
                        tau_proto::Effort::XHigh,
                    ]
                } else {
                    vec![tau_proto::Effort::Off]
                },
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

/// Run one finite backend attempt while owning all public event
/// sampling/writes.
#[allow(clippy::too_many_arguments)] // Boundary inputs are intentionally explicit and have distinct owners.
pub fn run_prompt_attempt<W: std::io::Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ChatCompletionsProvider,
    model: &ChatCompletionsModel,
    debug_provider_requests: bool,
    writer: &mut tau_proto::PeerOutputWriter<W>,
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
) -> PromptAttemptOutcome {
    let compat = model.compat.unwrap_or(provider.compat);
    let config = tau_provider_chat_completions::AttemptConfig {
        base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        max_output_tokens: provider.max_output_tokens,
        extra_body: provider.extra_body.clone(),
        compat: tau_provider_chat_completions::AttemptCompat {
            stream_options: compat.stream_options,
            parallel_tool_calls: compat.parallel_tool_calls,
            prompt_cache_key: compat.prompt_cache_key,
            reasoning_effort: compat.reasoning_effort,
            max_completion_tokens: compat.max_completion_tokens,
        },
    };
    let wire_model = tau_provider_chat_completions::AttemptModel {
        id: model.id.clone(),
    };
    let mut sampler = ResponseSampler::new();
    let outcome = tau_provider_chat_completions::run_attempt(
        prompt,
        &config,
        &wire_model,
        debug_provider_requests,
        &mut |progress| sampler.emit_if_due(agent_prompt_id, prompt, progress, writer),
        is_canceled,
        network,
    );
    match outcome {
        tau_provider_chat_completions::AttemptOutcome::Completed(success) => {
            sampler.latest_items = success.progress_items;
            sampler.latest_bytes = success.response_bytes_received;
            sampler.flush(agent_prompt_id, prompt, writer);
            PromptAttemptOutcome::Finished(Box::new(finished(
                agent_prompt_id,
                prompt,
                &provider.base_url,
                success.output_items,
                success.stop_reason,
                None,
                None,
                success.usage,
            )))
        }
        tau_provider_chat_completions::AttemptOutcome::Retryable { decision, progress } => {
            PromptAttemptOutcome::Retry { decision, progress }
        }
        tau_provider_chat_completions::AttemptOutcome::Canceled { progress } => {
            PromptAttemptOutcome::Canceled { progress }
        }
        tau_provider_chat_completions::AttemptOutcome::Terminal(failure) => {
            PromptAttemptOutcome::Terminal {
                finished: Box::new(finished(
                    agent_prompt_id,
                    prompt,
                    &provider.base_url,
                    Vec::new(),
                    failure.stop_reason,
                    Some(failure.message),
                    failure.failure_kind,
                    None,
                )),
                progress: failure.progress,
            }
        }
    }
}

/// Extension-owned interpretation of one backend attempt.
pub enum PromptAttemptOutcome {
    /// The prompt completed successfully.
    Finished(Box<tau_proto::ProviderResponseFinished>),
    /// A deterministic terminal failure ended an attempt.
    Terminal {
        /// Final protocol event for the failed attempt.
        finished: Box<tau_proto::ProviderResponseFinished>,
        /// Semantic output parsed before the terminal failure.
        progress: tau_provider_chat_completions::SemanticProgress,
    },
    /// The prompt remains pending for extension-owned scheduling.
    Retry {
        /// Structured scheduler facts.
        decision: tau_provider::retry_policy::RetryDecision,
        /// Semantic output parsed before the retryable failure.
        progress: tau_provider_chat_completions::SemanticProgress,
    },
    /// The active attempt observed cancellation.
    Canceled {
        /// Semantic output parsed before cancellation.
        progress: tau_provider_chat_completions::SemanticProgress,
    },
}

#[allow(clippy::too_many_arguments)] // Mirrors the stable protocol event without an intermediate policy DTO.
fn finished(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    base_url: &str,
    output_items: Vec<tau_proto::ContextItem>,
    stop_reason: tau_proto::ProviderStopReason,
    error: Option<String>,
    failure_kind: Option<tau_proto::ProviderFailureKind>,
    usage: Option<tau_proto::ProviderTokenUsage>,
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
            kind: tau_proto::ProviderBackendKind::ChatCompletions,
            base_url: base_url.to_owned(),
            transport: tau_proto::ProviderBackendTransport::HttpSse,
            stale_chain_fallback: false,
        }),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}
