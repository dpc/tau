//! Shared provider dispatch after private referenced-image materialization.

use tau_provider::local_summary_compaction::CanonicalPrefixAdmission;

use super::*;

/// Resolves typed image inputs before any selected adapter or credential work.
pub(super) fn handle_prompt_backend<R, S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    backend: &PromptBackend,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut S,
    retry_ctx: &mut R,
    context: ChatGptPromptExecutionContext<'_>,
    on_quota: &mut impl FnMut(&tau_provider_codex::RollingQuotaObservation),
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    // Admit the canonical descriptor-backed history before materialization.
    // The same cap must not be charged again against the expanded private copy.
    let canonical_prefix_admission = if let PromptBackend::ChatCompletions {
        provider,
        model_index,
    } = backend
        && prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
        && provider.models[*model_index].supports_image_tool_results()
        && prompt.has_image_references()
        && let Some(config) = chat_completions::resolved_local_summary_compaction(
            provider.models[*model_index].local_summary_compaction,
            &provider.models[*model_index],
        ) {
        if retry_ctx.is_aborted() {
            return finish_canceled_attempt(
                agent_prompt_id,
                prompt,
                writer,
                false,
                context.prior_backend.cloned(),
                context.logical_attempt.provider_attempt(),
            );
        }
        match CanonicalPrefixAdmission::new(prompt, config) {
            Ok(admission) => Some(admission),
            Err(reason) => {
                if retry_ctx.is_aborted() {
                    return finish_canceled_attempt(
                        agent_prompt_id,
                        prompt,
                        writer,
                        false,
                        context.prior_backend.cloned(),
                        context.logical_attempt.provider_attempt(),
                    );
                }
                let mut finished = simple_finished(
                    agent_prompt_id.clone(),
                    prompt.agent_id.clone(),
                    prompt.originator.clone(),
                    reason,
                );
                finished.failure_kind = Some(tau_proto::ProviderFailureKind::RequestRejected);
                finished.backend = context.prior_backend.cloned();
                finished.provider_attempt = context.logical_attempt.provider_attempt();
                return finish_terminal_attempt(agent_prompt_id, prompt, writer, finished, false);
            }
        }
    } else {
        None
    };
    let consumes_images = match backend {
        PromptBackend::Responses(config) => config.supports_image_tool_results(),
        PromptBackend::ChatCompletions {
            provider,
            model_index,
        } => provider.models[*model_index].supports_image_tool_results(),
        PromptBackend::Grok {
            profile,
            model_index,
        } => profile.models[*model_index].native_tool_images,
        PromptBackend::ChatGptPlan { .. }
        | PromptBackend::PublicResponses { .. }
        | PromptBackend::Unavailable { .. } => false,
    };
    let mut resolved_prompt = None;
    if consumes_images
        && prompt.has_image_references()
        && let Some(resolver) = context.image_resolution
    {
        let mut resolved = prompt.clone();
        if resolver
            .resolve(&mut resolved.context, &resolved.session_id, retry_ctx)
            .is_ok()
        {
            resolved_prompt = Some(resolved);
        }
    }
    let prompt = resolved_prompt.as_ref().unwrap_or(prompt);
    if consumes_images && prompt.has_image_references() {
        if retry_ctx.is_aborted() {
            return finish_canceled_attempt(
                agent_prompt_id,
                prompt,
                writer,
                false,
                None,
                context.logical_attempt.provider_attempt(),
            );
        }
        let mut finished = simple_finished(
            agent_prompt_id.clone(),
            prompt.agent_id.clone(),
            prompt.originator.clone(),
            "prepared image content is unavailable or invalid",
        );
        finished.failure_kind = Some(tau_proto::ProviderFailureKind::RequestRejected);
        finished.provider_attempt = context.logical_attempt.provider_attempt();
        return finish_terminal_attempt(agent_prompt_id, prompt, writer, finished, false);
    }
    match backend {
        PromptBackend::ChatGptPlan {
            profile,
            model_index,
        } => {
            let selected = profile
                .credential
                .as_ref()
                .ok_or("ChatGPT registration is missing")?;
            let session = profile
                .session
                .as_ref()
                .ok_or("ChatGPT credential owner unavailable")?;
            let credential = session.credential(selected, context.runtime.network(), &mut || {
                TurnAbort::is_aborted(retry_ctx)
            });
            let mut provider = profile.responses();
            match credential {
                Ok(credential) => provider.api_key = credential.access_token()?.to_owned(),
                Err(reason) => {
                    return public_backends::finish_chatgpt_credential_failure(
                        agent_prompt_id,
                        prompt,
                        &provider,
                        reason,
                        writer,
                        retry_ctx,
                        context,
                    );
                }
            }
            handle_public_responses_backend(
                agent_prompt_id,
                prompt,
                &provider,
                &provider.models[*model_index],
                responses::Route::ChatGptPlan,
                writer,
                retry_ctx,
                context,
            )
        }
        PromptBackend::Grok {
            profile,
            model_index,
        } => {
            let provider = profile.responses();
            handle_public_responses_backend(
                agent_prompt_id,
                prompt,
                &provider,
                &provider.models[*model_index],
                responses::Route::Grok(&profile.models[*model_index]),
                writer,
                retry_ctx,
                context,
            )
        }
        PromptBackend::Unavailable { .. } => Ok(Some(PromptAttemptRetry {
            decision: RetryDecision::new(RetryClass::Auth),
            live_detail: None,
            canonical_unauthorized: false,
            terminal_backend: None,
        })),
        PromptBackend::Responses(config) => handle_prompt(
            agent_prompt_id,
            config,
            prompt,
            writer,
            retry_ctx,
            context,
            on_quota,
        ),
        PromptBackend::ChatCompletions {
            provider,
            model_index,
        } => handle_chat_completions_backend(
            agent_prompt_id,
            prompt,
            canonical_prefix_admission,
            provider,
            &provider.models[*model_index],
            writer,
            retry_ctx,
            context,
        ),
        PromptBackend::PublicResponses {
            provider,
            model_index,
        } => handle_public_responses_backend(
            agent_prompt_id,
            prompt,
            provider,
            &provider.models[*model_index],
            responses::Route::Generic,
            writer,
            retry_ctx,
            context,
        ),
    }
}
