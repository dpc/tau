//! Extension policy adapter for one finite Chat Completions attempt.

use tau_provider::local_summary_compaction::Config as SummaryCompactionConfig;

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
            for tag in &model.tags {
                if !tags.contains(tag) {
                    tags.push(tag.clone());
                }
            }
            let compat = model.compat.as_ref().unwrap_or(&provider.compat);
            let builtin = super::builtin_estimated_prices(&model.id);
            let builtin_uncached = builtin.map(|prices| prices.0);
            let builtin_cached = builtin.map(|prices| prices.1);
            let builtin_output = builtin.map(|prices| prices.2);
            tau_proto::ProviderModelInfo {
                id: tau_proto::ModelId::new(provider_name.clone(), model.id.clone()),
                display_name: model.display_name.clone(),
                tags,
                supported_tool_types: vec![tau_proto::ToolType::Function],
                input_modalities: model.input_modalities.clone(),
                tool_result_modalities: model.tool_result_modalities.clone(),
                supports_parallel_tool_calls: model.supports_parallel_tool_calls,
                default_affinity: 0,
                context_window: model.context_window,
                efforts: compat.reasoning_effort.as_ref().map_or_else(
                    || vec![tau_proto::Effort::Off],
                    |config| config.efforts.canonical(),
                ),
                verbosities: vec![tau_proto::Verbosity::Medium],
                thinking_summaries: vec![tau_proto::ThinkingSummary::Off],
                supports_compaction: false,
                supports_standalone_compaction: super::resolved_local_summary_compaction(
                    model.local_summary_compaction,
                    model.context_window,
                )
                .is_some(),
                standalone_compaction_threshold: super::resolved_local_summary_compaction(
                    model.local_summary_compaction,
                    model.context_window,
                )
                .map(SummaryCompactionConfig::proactive_threshold),
                standalone_compaction_prefix_budget: super::resolved_local_summary_compaction(
                    model.local_summary_compaction,
                    model.context_window,
                )
                .map(SummaryCompactionConfig::max_input_bytes),
                cache_policy: model.cache_contract.map(|contract| {
                    contract
                        .runtime_policy()
                        .expect("deserialized cache contract remains valid")
                }),
                est_uncached_input_cost_1m_usd: model
                    .est_uncached_input_cost_1m_usd
                    .or(builtin_uncached),
                est_cached_input_cost_1m_usd: model.est_cached_input_cost_1m_usd.or(builtin_cached),
                est_cache_write_input_cost_1m_usd: model.est_cache_write_input_cost_1m_usd,
                est_output_cost_1m_usd: model.est_output_cost_1m_usd.or(builtin_output),
                est_cache_storage_cost_1m_token_hour_usd: model
                    .est_cache_storage_cost_1m_token_hour_usd,
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
        local_summary_compaction: super::resolved_local_summary_compaction(
            model.local_summary_compaction,
            model.context_window,
        ),
        extra_body: provider.extra_body.clone(),
        compat: lower_compat(compat),
    };
    let wire_model = tau_provider_chat_completions::AttemptModel {
        id: model.id.clone(),
        supports_image_tool_results: model
            .input_modalities
            .contains(&tau_proto::InputModality::Image)
            && model
                .tool_result_modalities
                .contains(&tau_proto::InputModality::Image),
    };
    let mut sampler = ResponseSampler::new();
    let outcome = tau_provider_chat_completions::run_attempt(
        prompt,
        &config,
        &wire_model,
        debug_provider_requests,
        &mut |update| match update {
            tau_provider_chat_completions::AttemptUpdate::Dispatched(at) => {
                sampler.mark_dispatched(at);
            }
            tau_provider_chat_completions::AttemptUpdate::Progress(progress) => {
                sampler.emit_if_due(agent_prompt_id, prompt, progress, writer);
            }
        },
        is_canceled,
        network,
    );
    match outcome {
        tau_provider_chat_completions::AttemptOutcome::Completed(mut success) => {
            if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction {
                if success.stop_reason != tau_proto::ProviderStopReason::EndTurn {
                    return PromptAttemptOutcome::Terminal {
                        finished: Box::new(finished(
                            agent_prompt_id,
                            prompt,
                            &provider.base_url,
                            Vec::new(),
                            tau_proto::ProviderStopReason::Error,
                            Some("summary compactor did not complete its output".to_owned()),
                            Some(tau_proto::ProviderFailureKind::RequestRejected),
                            success.usage,
                        )),
                        progress: tau_provider_chat_completions::SemanticProgress::Parsed,
                    };
                }
                match validate_resolved_narrative_output(
                    success.output_items,
                    super::resolved_local_summary_compaction(
                        model.local_summary_compaction,
                        model.context_window,
                    )
                    .expect("standalone compaction is dispatched only for a supported model"),
                ) {
                    Ok(output) => success.output_items = vec![output],
                    Err(error) => {
                        return PromptAttemptOutcome::Terminal {
                            finished: Box::new(finished(
                                agent_prompt_id,
                                prompt,
                                &provider.base_url,
                                Vec::new(),
                                tau_proto::ProviderStopReason::Error,
                                Some(error),
                                Some(tau_proto::ProviderFailureKind::RequestRejected),
                                success.usage,
                            )),
                            progress: tau_provider_chat_completions::SemanticProgress::Parsed,
                        };
                    }
                }
            }
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
        tau_provider_chat_completions::AttemptOutcome::Retryable {
            decision: _,
            progress,
        } if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
            && progress == tau_provider_chat_completions::SemanticProgress::Parsed =>
        {
            PromptAttemptOutcome::Terminal {
                finished: Box::new(finished(
                    agent_prompt_id,
                    prompt,
                    &provider.base_url,
                    Vec::new(),
                    tau_proto::ProviderStopReason::Error,
                    Some("summary compactor failed after semantic output".to_owned()),
                    Some(tau_proto::ProviderFailureKind::Unknown),
                    None,
                )),
                progress,
            }
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

/// Lower serialized route capabilities into one backend attempt.
fn lower_compat(
    compat: super::ChatCompletionsCompat,
) -> tau_provider_chat_completions::AttemptCompat {
    tau_provider_chat_completions::AttemptCompat {
        stream_options: compat.stream_options,
        parallel_tool_calls: compat.parallel_tool_calls,
        prompt_cache: compat.openai_prompt_cache.map(|cache| match cache.key {
            crate::OpenAiPromptCacheKey::Agent => match cache.policy {
                super::OpenAiPromptCachePolicy::Legacy { retention } => {
                    tau_provider_chat_completions::PromptCache::Legacy {
                        retention: match retention {
                            crate::OpenAiPromptCacheRetention::InMemory => {
                                tau_provider_chat_completions::PromptCacheRetention::InMemory
                            }
                            crate::OpenAiPromptCacheRetention::Hours24 => {
                                tau_provider_chat_completions::PromptCacheRetention::Hours24
                            }
                        },
                    }
                }
                super::OpenAiPromptCachePolicy::Explicit { .. } => {
                    tau_provider_chat_completions::PromptCache::ExplicitSystemPrompt
                }
            },
        }),
        reasoning_effort: compat
            .reasoning_effort
            .and_then(|config| match config.wire {
                super::ChatCompletionsReasoningEffortWire::OpenAi => {
                    Some(tau_provider_chat_completions::ReasoningEffortWire::OpenAi)
                }
                super::ChatCompletionsReasoningEffortWire::Literal => {
                    Some(tau_provider_chat_completions::ReasoningEffortWire::Literal)
                }
                super::ChatCompletionsReasoningEffortWire::Omit => None,
            }),
        reasoning_replay: match compat.reasoning_replay {
            super::ChatCompletionsReasoningReplay::ReasoningContent => {
                tau_provider_chat_completions::ReasoningReplay::ReasoningContent
            }
            super::ChatCompletionsReasoningReplay::Reasoning => {
                tau_provider_chat_completions::ReasoningReplay::Reasoning
            }
            super::ChatCompletionsReasoningReplay::Both => {
                tau_provider_chat_completions::ReasoningReplay::Both
            }
        },
        single_initial_system_message: compat.single_initial_system_message,
        max_completion_tokens: compat.max_completion_tokens,
        cache_usage: match compat.cache_usage {
            super::CacheUsageCompat::None => tau_provider_chat_completions::CacheUsageCompat::None,
            super::CacheUsageCompat::OpenAi => {
                tau_provider_chat_completions::CacheUsageCompat::OpenAi
            }
            super::CacheUsageCompat::DeepSeek => {
                tau_provider_chat_completions::CacheUsageCompat::DeepSeek
            }
        },
    }
}

pub(crate) fn validate_resolved_narrative_output(
    items: Vec<tau_proto::ContextItem>,
    config: SummaryCompactionConfig,
) -> Result<tau_proto::ContextItem, String> {
    let mut message = None;
    let mut reasoning_bytes = 0_u64;
    for item in &items {
        match item {
            tau_proto::ContextItem::Message(item) if message.is_none() => message = Some(item),
            tau_proto::ContextItem::ReasoningText(item) => {
                reasoning_bytes = reasoning_bytes
                    .checked_add(u64::try_from(item.text.len()).unwrap_or(u64::MAX))
                    .ok_or_else(|| {
                        "summary compactor reasoning exceeds its byte limit".to_owned()
                    })?;
                if config.max_output_bytes() < reasoning_bytes {
                    return Err("summary compactor reasoning exceeds its byte limit".to_owned());
                }
            }
            _ => return Err("summary compactor returned unsupported output".to_owned()),
        }
    }
    let Some(message) = message else {
        return Err("summary compactor did not return exactly one message".to_owned());
    };
    if message.role != tau_proto::ContextRole::Assistant {
        return Err("summary compactor returned an invalid role".to_owned());
    }
    let text = message
        .content
        .iter()
        .map(|part| match part {
            tau_proto::ContentPart::Text { text }
            | tau_proto::ContentPart::HarnessInternalText { text } => text.as_str(),
        })
        .collect::<String>();
    if text.trim().is_empty()
        || u64::try_from(text.len()).unwrap_or(u64::MAX) > config.max_output_bytes()
    {
        return Err("summary compactor output is empty or exceeds its byte limit".to_owned());
    }
    // Only the provider's bounded final assistant text crosses the private
    // extension seam; reasoning remains absent from semantic history.
    Ok(tau_proto::ContextItem::LocalCompactionNarrative(
        tau_proto::LocalCompactionNarrativeItem { narrative: text },
    ))
}

#[cfg(test)]
fn validate_narrative_output(
    items: Vec<tau_proto::ContextItem>,
    config: super::LocalSummaryCompactionConfig,
) -> Result<tau_proto::ContextItem, String> {
    validate_resolved_narrative_output(
        items,
        config
            .validated_for(config.context_window_tokens.get())
            .expect("test compaction config is valid"),
    )
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
        automatic_compaction_decision: None,
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
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
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
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

#[cfg(test)]
mod tests;
