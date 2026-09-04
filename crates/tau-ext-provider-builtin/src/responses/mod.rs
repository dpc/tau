//! Extension-owned public Responses profiles and finite attempt adapter.

use crate::report_sink::ProviderReportSink;
mod prompt_cache;
mod sampling;
#[cfg(test)]
mod tests;

#[cfg(test)]
use std::cell::RefCell;
use std::num::NonZeroU32;

pub use prompt_cache::{
    OpenAiPromptCache, OpenAiPromptCacheBoundary, OpenAiPromptCacheMode, OpenAiPromptCacheOptions,
    OpenAiPromptCacheTtl,
};
use serde::de::Error as SerdeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tau_proto::{ModelName, NativeReasoningEffort, ProviderModelInfo, ProviderName, TokenCount};
use tau_provider::local_summary_compaction::{
    Config as SummaryCompactionConfig, ConfigError as SummaryCompactionConfigError,
};

use self::sampling::ResponsesResponseSampler;
use crate::OpenAiPromptCacheKey;

#[cfg(test)]
thread_local! {
    /// Values observed at the actual adapter call seam in the current test.
    static FORWARDED_DEBUG_CAPTURE_POLICY: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
}

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

impl ResponsesProvider {
    /// Validate every model's optional summary limits against its own context
    /// window.
    pub(crate) fn validate_local_summary_compaction(
        &self,
    ) -> Result<(), SummaryCompactionConfigError> {
        self.models
            .iter()
            .try_for_each(|model| match model.local_summary_compaction {
                Some(config) => config.validated_for(model.context_window).map(|_| ()),
                None => Ok(()),
            })
    }
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
    pub efforts: Option<ResponsesNativeReasoningEfforts>,
    /// Optional model-specific public Responses wire compatibility override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<ResponsesCompat>,
    /// Optional user-facing label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Total model context window.
    #[serde(default = "default_context_window")]
    pub context_window: TokenCount,
    /// Optional maximum legal input tokens for this route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<TokenCount>,
    /// Optional maximum output capability for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<NonZeroU32>,
    /// Model-specific tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<tau_proto::ModelTag>,
    /// Whether the model may issue several Function calls in one response.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub supports_parallel_tool_calls: bool,
    /// Optional independent overrides for Tau-owned summary compaction limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_summary_compaction: Option<super::chat_completions::LocalSummaryCompactionConfig>,
    /// Optional operator-declared runtime cache contract for this exact model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_contract: Option<crate::ProviderCacheContract>,
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

impl ResponsesModel {
    /// Apply the model capability to the provider-wide output policy.
    fn requested_output_tokens(&self, policy_cap: u32) -> u32 {
        if policy_cap == 0 {
            return 0;
        }
        self.max_output_tokens
            .map_or(policy_cap, |capability| policy_cap.min(capability.get()))
    }
}

/// Serialized public Responses request controls.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesCompat {
    /// Exact OpenAI prompt-cache controls accepted by this route.
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
pub struct ResponsesNativeReasoningEfforts {
    /// Unique configured levels in canonical UI cycling order.
    levels: Vec<NativeReasoningEffort>,
}

impl ResponsesNativeReasoningEfforts {
    /// Returns the validated levels in canonical UI cycling order.
    #[must_use]
    pub fn as_slice(&self) -> &[NativeReasoningEffort] {
        &self.levels
    }
}

impl Serialize for ResponsesNativeReasoningEfforts {
    /// Serializes validated effort levels as the profile's direct array value.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.levels.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ResponsesNativeReasoningEfforts {
    /// Deserializes and validates the profile's direct effort array value.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(Vec::deserialize(deserializer)?).map_err(SerdeError::custom)
    }
}

impl TryFrom<Vec<NativeReasoningEffort>> for ResponsesNativeReasoningEfforts {
    type Error = &'static str;

    /// Validates unique configured levels and stores them in canonical order.
    fn try_from(configured: Vec<NativeReasoningEffort>) -> Result<Self, Self::Error> {
        if configured
            .iter()
            .enumerate()
            .any(|(index, effort)| configured[..index].contains(effort))
        {
            return Err("Responses model efforts must not contain duplicates");
        }

        Ok(Self {
            levels: NativeReasoningEffort::ALL
                .into_iter()
                .filter(|effort| configured.contains(effort))
                .collect(),
        })
    }
}

fn model_efforts(model: &ResponsesModel) -> Vec<NativeReasoningEffort> {
    model.efforts.as_ref().map_or_else(
        || NativeReasoningEffort::ALL.to_vec(),
        |efforts| efforts.as_slice().to_vec(),
    )
}

fn default_context_window() -> TokenCount {
    TokenCount::new(128_000)
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
                hosted_tool_capabilities: Vec::new(),
                supported_tool_types: vec![tau_proto::ToolType::Function],
                input_modalities: Vec::new(),
                tool_result_modalities: Vec::new(),
                supports_parallel_tool_calls: model.supports_parallel_tool_calls,
                default_affinity: 0,
                context_window: model.context_window,
                max_input_tokens: model.max_input_tokens,
                max_output_tokens: model
                    .max_output_tokens
                    .map(|tokens| TokenCount::new(u64::from(tokens.get()))),
                efforts: tau_proto::ReasoningEffortCapability::mapped(model_efforts(model)),
                verbosities: vec![tau_proto::Verbosity::Medium],
                thinking_summaries: vec![tau_proto::ThinkingSummary::Off],
                supports_compaction: false,
                supports_standalone_compaction: resolved_summary_config(model).is_some(),
                standalone_compaction_generation_negative: false,
                standalone_compaction_threshold: None,
                standalone_compaction_prefix_budget: resolved_summary_config(model)
                    .and_then(SummaryCompactionConfig::max_input_bytes),
                cache_policy: model.cache_contract.map(|contract| {
                    contract
                        .runtime_policy()
                        .expect("deserialized cache contract remains valid")
                }),
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
pub fn run_prompt_attempt<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ResponsesProvider,
    model: &ResponsesModel,
    debug_provider_requests: bool,
    writer: &mut S,
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
    provider_attempt: tau_proto::ProviderAttempt,
) -> PromptAttemptOutcome {
    let summary_config = resolved_summary_config(model);
    let compact_prompt = if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction {
        let Some(config) = summary_config else {
            return invalid_compaction(
                agent_prompt_id,
                prompt,
                provider,
                "model context window is too small for summary compaction",
                false,
                provider_attempt,
            );
        };
        match materialize_summary_prompt(prompt, config) {
            Ok(compact) => Some(compact),
            Err(error) => {
                return invalid_compaction(
                    agent_prompt_id,
                    prompt,
                    provider,
                    error,
                    false,
                    provider_attempt,
                );
            }
        }
    } else {
        None
    };
    let effective_prompt = compact_prompt.as_ref().unwrap_or(prompt);
    let compat = model.compat.unwrap_or(provider.compat);
    let config = tau_provider_responses::AttemptConfig {
        base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        max_output_tokens: attempt_output_tokens(
            model.requested_output_tokens(provider.max_output_tokens),
            summary_config,
            compact_prompt.is_some(),
        ),
        transport: provider.transport,
        prompt_cache: compat.openai_prompt_cache.map(|cache| match cache.key {
            OpenAiPromptCacheKey::Agent => tau_provider_responses::PromptCachePolicy {
                mode: match cache.options.mode {
                    OpenAiPromptCacheMode::Implicit => {
                        tau_provider_responses::PromptCacheMode::Implicit
                    }
                    OpenAiPromptCacheMode::Explicit => {
                        tau_provider_responses::PromptCacheMode::Explicit
                    }
                },
                ttl: match cache.options.ttl {
                    OpenAiPromptCacheTtl::Minutes30 => {
                        tau_provider_responses::PromptCacheTtl::Minutes30
                    }
                },
            },
        }),
    };
    let model = tau_provider_responses::AttemptModel {
        id: model.id.clone(),
    };
    let mut sampler = ResponsesResponseSampler::new();
    let mut backend_reached = false;
    let outcome =
        forward_debug_capture_policy(debug_provider_requests, |debug_provider_requests| {
            tau_provider_responses::run_attempt_with_debug(
                effective_prompt,
                &config,
                &model,
                debug_provider_requests,
                &mut |update| match update {
                    tau_provider_responses::AttemptUpdate::Dispatched(dispatched_at) => {
                        backend_reached = true;
                        sampler.mark_dispatched(dispatched_at);
                    }
                    tau_provider_responses::AttemptUpdate::Progress(progress) => {
                        sampler.emit_if_due(agent_prompt_id, prompt, progress, writer);
                    }
                },
                is_canceled,
                network,
            )
        });
    match outcome {
        tau_provider_responses::AttemptOutcome::Completed(mut success) => {
            if let Some(config) = summary_config.filter(|_| compact_prompt.is_some()) {
                match success.stop_reason {
                    tau_proto::ProviderStopReason::EndTurn => {
                        success.output_items =
                            match validate_responses_narrative_output(success.output_items, config)
                            {
                                Ok(output) => vec![output],
                                Err(error) => {
                                    return invalid_compaction(
                                        agent_prompt_id,
                                        prompt,
                                        provider,
                                        &error,
                                        backend_reached,
                                        provider_attempt,
                                    );
                                }
                            };
                    }
                    tau_proto::ProviderStopReason::Length => {
                        // Preserve the incomplete terminal unchanged. The
                        // harness records it outside
                        // context and closes the transaction.
                    }
                    _ => {
                        return invalid_compaction(
                            agent_prompt_id,
                            prompt,
                            provider,
                            "summary compactor did not complete its output",
                            backend_reached,
                            provider_attempt,
                        );
                    }
                }
            }
            sampler.flush_from(agent_prompt_id, prompt, &success, writer);
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
                backend_reached,
                provider_attempt,
            )))
        }
        tau_provider_responses::AttemptOutcome::Retryable { decision, progress } => {
            if summary_retry_is_terminal(compact_prompt.is_some(), &progress) {
                return invalid_compaction(
                    agent_prompt_id,
                    prompt,
                    provider,
                    "summary compactor failed after semantic output",
                    backend_reached,
                    provider_attempt,
                );
            }
            PromptAttemptOutcome::Retry {
                decision,
                progress,
                backend_reached,
            }
        }
        tau_provider_responses::AttemptOutcome::Canceled { progress } => {
            PromptAttemptOutcome::Canceled {
                progress,
                backend_reached,
            }
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
                    backend_reached,
                    provider_attempt,
                )),
                progress: failure.progress,
            }
        }
    }
}

fn summary_retry_is_terminal(
    is_compaction: bool,
    progress: &tau_provider_responses::AttemptProgress,
) -> bool {
    is_compaction && progress.has_timed_semantic_output
}

fn attempt_output_tokens(
    ordinary: u32,
    summary: Option<SummaryCompactionConfig>,
    is_compaction: bool,
) -> u32 {
    summary
        .filter(|_| is_compaction)
        .map_or(ordinary, SummaryCompactionConfig::max_output_tokens)
}

fn materialize_summary_prompt(
    prompt: &tau_proto::AgentPromptCreated,
    config: SummaryCompactionConfig,
) -> Result<tau_proto::AgentPromptCreated, &'static str> {
    let prefix_bytes =
        tau_provider::local_summary_compaction::historical_prefix_json_bytes(&prompt.context);
    if config
        .max_input_bytes()
        .is_some_and(|budget| prefix_bytes.is_none_or(|bytes| budget < bytes))
    {
        return Err("summary compaction prefix exceeds the published safe budget");
    }
    let mut compact = prompt.clone();
    tau_provider::local_summary_compaction::replace_trailing_trigger(&mut compact.context)?;
    Ok(compact)
}

fn resolved_summary_config(model: &ResponsesModel) -> Option<SummaryCompactionConfig> {
    let config = match model.local_summary_compaction {
        Some(config) => config
            .validated_for(model.context_window)
            .expect("validated provider profile must retain valid summary limits"),
        None => SummaryCompactionConfig::default_for(model.context_window.get()),
    };
    model.max_output_tokens.map_or(config, |capability| {
        config.and_then(|config| {
            config.capped_output_tokens(TokenCount::new(u64::from(capability.get())))
        })
    })
}

fn validate_responses_narrative_output(
    items: Vec<tau_proto::ContextItem>,
    config: SummaryCompactionConfig,
) -> Result<tau_proto::ContextItem, String> {
    // Public Responses represents one reasoning stream twice: visible bounded
    // reasoning text and an opaque provider replay item. The summary validator
    // bounds and discards the text; discard its replay-only twin before applying
    // the shared one-message semantic contract.
    super::chat_completions::validate_resolved_narrative_output(
        items
            .into_iter()
            .filter(|item| !matches!(item, tau_proto::ContextItem::Reasoning(_)))
            .collect(),
        config,
    )
}

fn invalid_compaction(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ResponsesProvider,
    message: &str,
    backend_reached: bool,
    provider_attempt: tau_proto::ProviderAttempt,
) -> PromptAttemptOutcome {
    PromptAttemptOutcome::Terminal {
        finished: Box::new(finished(
            agent_prompt_id,
            prompt,
            provider,
            Vec::new(),
            tau_proto::ProviderStopReason::Error,
            Some(message.to_owned()),
            Some(tau_proto::ProviderFailureKind::RequestRejected),
            None,
            None,
            backend_reached,
            provider_attempt,
        )),
        progress: tau_provider_responses::AttemptProgress {
            output_items: Vec::new(),
            response_bytes_received: 0,
            has_timed_semantic_output: false,
        },
    }
}

/// Forward the durable-session capture decision unchanged across the extension
/// boundary into the generic Responses adapter.
fn forward_debug_capture_policy<T>(
    debug_provider_requests: bool,
    run: impl FnOnce(bool) -> T,
) -> T {
    #[cfg(test)]
    FORWARDED_DEBUG_CAPTURE_POLICY.with(|observed| {
        observed.borrow_mut().push(debug_provider_requests);
    });
    run(debug_provider_requests)
}

/// Drain policy values observed at the real adapter invocation seam.
#[cfg(test)]
fn take_forwarded_debug_capture_policy() -> Vec<bool> {
    FORWARDED_DEBUG_CAPTURE_POLICY.with(|observed| std::mem::take(&mut *observed.borrow_mut()))
}

/// Extension-owned outcome for the generic Responses finite attempt.
pub enum PromptAttemptOutcome {
    /// Completion produced a terminal provider response.
    Finished(Box<tau_proto::ProviderResponseFinished>),
    /// Scheduler may retry from the full local transcript.
    Retry {
        decision: tau_provider::retry_policy::RetryDecision,
        progress: tau_provider_responses::AttemptProgress,
        /// Whether this attempt crossed the backend dispatch boundary.
        backend_reached: bool,
    },
    /// Cancellation won the finite attempt.
    Canceled {
        progress: tau_provider_responses::AttemptProgress,
        /// Whether this attempt crossed the backend dispatch boundary.
        backend_reached: bool,
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
    backend_reached: bool,
    provider_attempt: tau_proto::ProviderAttempt,
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
        compaction_output_tokens: None,
        backend: backend_reached.then_some(tau_proto::ProviderBackend {
            kind: tau_proto::ProviderBackendKind::PublicResponses,
            base_url: provider.base_url.clone(),
            transport: backend_transport(provider),
            stale_chain_fallback: false,
        }),
        provider_response_id,
        ws_pool_delta: None,
        provider_attempt,
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
