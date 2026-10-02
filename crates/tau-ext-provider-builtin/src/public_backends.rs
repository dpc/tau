//! Finite public backend dispatch and shared terminal/retry reporting.

use std::error::Error;

use crate::chat_completions::{
    PromptAttemptOutcome as ChatCompletionsAttemptOutcome, run_prompt_attempt,
};
use crate::responses::{self, PromptAttemptOutcome as ResponsesAttemptOutcome};
use crate::{
    CancellationFinishPolicy, ChatCompletionsModel, ChatCompletionsProvider,
    ChatGptPromptExecutionContext, PromptAttemptRetry, ProviderReportSink, ResponsesModel,
    ResponsesProvider, TurnAbort, chat_completions_backend, finish_backend_attempt,
    finish_canceled_attempt, finish_retry_attempt, finish_terminal_attempt, observed_backend,
    responses_backend,
};

/// Runs one Chat Completions attempt and reports its terminal or retry outcome.
pub(crate) fn handle_chat_completions_backend<R, S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ChatCompletionsProvider,
    model: &ChatCompletionsModel,
    writer: &mut S,
    retry_ctx: &mut R,
    context: ChatGptPromptExecutionContext<'_>,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    if TurnAbort::is_aborted(retry_ctx) {
        return finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            false,
            context.prior_backend.cloned(),
            context.logical_attempt.provider_attempt(),
        );
    }
    let outcome = run_prompt_attempt(
        agent_prompt_id,
        prompt,
        provider,
        model,
        context.debug_provider_requests,
        writer,
        &mut || TurnAbort::is_aborted(retry_ctx),
        context.runtime.network(),
        context.logical_attempt.provider_attempt(),
    );
    match outcome {
        ChatCompletionsAttemptOutcome::Finished(finished) => finish_backend_attempt(
            agent_prompt_id,
            prompt,
            writer,
            retry_ctx,
            *finished,
            true,
            CancellationFinishPolicy {
                detail: "request canceled; discarding tentative provider output",
                retain_correlation: true,
            },
        ),
        ChatCompletionsAttemptOutcome::Terminal {
            mut finished,
            progress,
        } => {
            finished.backend = observed_backend(finished.backend.take(), context.prior_backend);
            finish_terminal_attempt(
                agent_prompt_id,
                prompt,
                writer,
                *finished,
                progress == tau_provider_chat_completions::SemanticProgress::Parsed,
            )
        }
        ChatCompletionsAttemptOutcome::Retry {
            decision,
            progress,
            backend_reached,
        } => finish_retry_attempt(
            agent_prompt_id,
            prompt,
            writer,
            decision,
            progress == tau_provider_chat_completions::SemanticProgress::Parsed,
            observed_backend(
                backend_reached.then(|| chat_completions_backend(provider)),
                context.prior_backend,
            ),
        ),
        ChatCompletionsAttemptOutcome::Canceled { progress, facts } => finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            progress == tau_provider_chat_completions::SemanticProgress::Parsed,
            observed_backend(
                facts
                    .backend_reached
                    .then(|| chat_completions_backend(provider)),
                context.prior_backend,
            ),
            facts.provider_attempt,
        ),
    }
}

/// Runs one public Responses attempt and reports its terminal or retry outcome.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_public_responses_backend<R, S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ResponsesProvider,
    model: &ResponsesModel,
    route: responses::Route<'_>,
    writer: &mut S,
    retry_ctx: &mut R,
    context: ChatGptPromptExecutionContext<'_>,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    if TurnAbort::is_aborted(retry_ctx) {
        return finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            false,
            context.prior_backend.cloned(),
            context.logical_attempt.provider_attempt(),
        );
    }
    let outcome = match route {
        responses::Route::Generic => responses::run_prompt_attempt(
            agent_prompt_id,
            prompt,
            provider,
            model,
            context.debug_provider_requests,
            writer,
            &mut || TurnAbort::is_aborted(retry_ctx),
            context.runtime.network(),
            context.logical_attempt.provider_attempt(),
        ),
        responses::Route::Grok(grok) => responses::run_grok_prompt_attempt(
            agent_prompt_id,
            prompt,
            provider,
            model,
            grok,
            context.debug_provider_requests,
            writer,
            &mut || TurnAbort::is_aborted(retry_ctx),
            context.runtime.network(),
            context.logical_attempt.provider_attempt(),
        ),
        responses::Route::ChatGptPlan => responses::run_selected_prompt_attempt(
            agent_prompt_id,
            prompt,
            provider,
            model,
            route,
            context.debug_provider_requests,
            writer,
            &mut || TurnAbort::is_aborted(retry_ctx),
            context.runtime.network(),
            context.logical_attempt.provider_attempt(),
        ),
    };
    match outcome {
        ResponsesAttemptOutcome::Finished(finished) => finish_backend_attempt(
            agent_prompt_id,
            prompt,
            writer,
            retry_ctx,
            *finished,
            true,
            CancellationFinishPolicy {
                detail: "request canceled; discarding tentative provider output",
                retain_correlation: true,
            },
        ),
        ResponsesAttemptOutcome::Terminal {
            mut finished,
            progress,
        } => {
            finished.backend = observed_backend(finished.backend.take(), context.prior_backend);
            finish_terminal_attempt(
                agent_prompt_id,
                prompt,
                writer,
                *finished,
                progress.has_timed_semantic_output,
            )
        }
        ResponsesAttemptOutcome::Retry {
            decision,
            progress,
            backend_reached,
            canonical_unauthorized,
        } => {
            let mut retry = finish_retry_attempt(
                agent_prompt_id,
                prompt,
                writer,
                decision,
                progress.has_timed_semantic_output,
                observed_backend(
                    backend_reached.then(|| responses_backend(provider)),
                    context.prior_backend,
                ),
            )?;
            if let Some(retry) = retry.as_mut() {
                retry.canonical_unauthorized =
                    matches!(route, responses::Route::Grok(_)) && canonical_unauthorized;
            }
            Ok(retry)
        }
        ResponsesAttemptOutcome::Canceled {
            progress,
            backend_reached,
        } => finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            progress.has_timed_semantic_output,
            observed_backend(
                backend_reached.then(|| responses_backend(provider)),
                context.prior_backend,
            ),
            context.logical_attempt.provider_attempt(),
        ),
    }
}

/// Credential failures stop the selected ChatGPT request rather than entering
/// a generic retry loop or changing accounts/billing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_chatgpt_credential_failure<R: TurnAbort, S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ResponsesProvider,
    reason: &str,
    writer: &mut S,
    retry_ctx: &mut R,
    context: ChatGptPromptExecutionContext<'_>,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>> {
    if TurnAbort::is_aborted(retry_ctx) {
        return finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            false,
            context.prior_backend.cloned(),
            context.logical_attempt.provider_attempt(),
        );
    }
    let ResponsesAttemptOutcome::Terminal { mut finished, .. } = responses::invalid_compaction(
        agent_prompt_id,
        prompt,
        provider,
        reason,
        false,
        context.logical_attempt.provider_attempt(),
    ) else {
        unreachable!("explicit request rejection is terminal")
    };
    finished.backend = observed_backend(finished.backend.take(), context.prior_backend);
    finish_terminal_attempt(agent_prompt_id, prompt, writer, *finished, false)
}
