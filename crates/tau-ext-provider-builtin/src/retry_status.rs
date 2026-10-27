//! Retry-status and exhausted-compaction reporting for provider prompt
//! attempts.

use super::*;

/// Reports one exhausted pre-output compaction cycle with durable typed
/// evidence.
pub(super) fn emit_compaction_retry_exhausted(
    job: PromptJob,
    attempt: tau_proto::ProviderAttempt,
    handle: &ClientHandle,
) -> ClientResult<()> {
    let mut finished = simple_finished(
        job.agent_prompt_id,
        job.prompt.agent_id,
        job.prompt.originator,
        "provider retry budget exhausted during standalone compaction",
    );
    finished.provider_attempt = attempt;
    finished.failure_kind = Some(tau_proto::ProviderFailureKind::TransientCompactionExhausted);
    finished.backend = job.observed_backend;
    if let Some(accounting) = job.terminal_accounting {
        finished.usage = accounting.usage;
        finished.provider_response_id = accounting.provider_response_id;
    }
    handle.send(HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(finished),
    ))
}

/// Emits the initiating user's transient status for one scheduled retry.
pub(super) fn emit_retry_status(
    job: &PromptJob,
    extension_instance: Option<&tau_proto::ExtensionName>,
    class: RetryClass,
    due: Instant,
    now: Instant,
    live_detail: Option<&str>,
    handle: &ClientHandle,
) -> ClientResult<()> {
    let text = retry_status_text(job, extension_instance, class, due, now, live_detail);
    handle.send(HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
            agent_prompt_id: job.agent_prompt_id.clone(),
            agent_id: job.prompt.agent_id.clone(),
            deltas: Vec::new(),
            compaction: None,
            status: Some(ProviderResponseStatusUpdate {
                text,
                clear_response: true,
                retry: Some(tau_proto::ProviderRetryStatus {
                    category: retry_class_provider_category(class),
                    attempt: if job.prompt.operation
                        == tau_proto::PromptOperation::StandaloneCompaction
                    {
                        u32::try_from(job.retry_state.attempts)
                            .expect("standalone scheduler preserves representable retry identities")
                    } else {
                        saturating_retry_attempt(job.retry_state.attempts)
                    },
                    next_retry_delay_secs: saturating_retry_delay(
                        due.checked_duration_since(now).unwrap_or(Duration::ZERO),
                    ),
                }),
                native_tool: None,
            }),
            response_stats: None,
            originator: job.prompt.originator.clone(),
        }),
    ))
}

/// Builds the initiating user's transient status for one scheduled retry.
pub(super) fn retry_status_text(
    job: &PromptJob,
    extension_instance: Option<&tau_proto::ExtensionName>,
    class: RetryClass,
    due: Instant,
    now: Instant,
    live_detail: Option<&str>,
) -> String {
    let delay = due.checked_duration_since(now).unwrap_or(Duration::ZERO);
    let delay_text = tau_proto::format_approximate_duration_secs(delay.as_secs());
    let reason = match (&job.backend, extension_instance) {
        (
            PromptBackend::Unavailable {
                login_required: Some(provider),
            },
            Some(extension_instance),
        ) => format!(
            "provider {provider} is not logged in; run {}",
            provider_login_command(extension_instance, provider)
        ),
        (
            PromptBackend::Unavailable {
                login_required: Some(_),
            },
            None,
        ) => class.public_reason().to_owned(),
        (
            PromptBackend::Unavailable {
                login_required: None,
            },
            _,
        )
        | (PromptBackend::Responses(_), _)
        | (PromptBackend::Grok { .. }, _)
        | (PromptBackend::ChatGptPlan { .. }, _)
        | (PromptBackend::ChatCompletions { .. }, _)
        | (PromptBackend::PublicResponses { .. }, _) => live_detail
            .map(|detail| format!("{}: {detail}", class.public_reason()))
            .unwrap_or_else(|| class.public_reason().to_owned()),
    };
    let recovery = if job.prompt.operation == tau_proto::PromptOperation::StandaloneCompaction {
        if class == RetryClass::Transport {
            "Compaction remains pending; Tau will retry connection failures in bounded bursts. Use :retry to retry now, or cancel to stop. Restart requires explicit compaction recovery."
        } else {
            "Compaction retries are limited; exhaustion pauses inference until explicit compaction recovery."
        }
    } else {
        "Tau will keep trying; cancel the prompt to stop."
    };
    format!(
        "{}; next attempt in about {} (attempt {}). {}",
        reason, delay_text, job.retry_state.attempts, recovery,
    )
}

/// Maps internal retry policy classes to their stable protocol categories.
pub(super) fn retry_class_provider_category(class: RetryClass) -> tau_proto::ProviderRetryCategory {
    match class {
        RetryClass::Transport => tau_proto::ProviderRetryCategory::Transport,
        RetryClass::Overload => tau_proto::ProviderRetryCategory::Overload,
        RetryClass::Throttle => tau_proto::ProviderRetryCategory::Throttle,
        RetryClass::UsageWindow => tau_proto::ProviderRetryCategory::UsageWindow,
        RetryClass::Account => tau_proto::ProviderRetryCategory::Account,
        RetryClass::Auth => tau_proto::ProviderRetryCategory::Auth,
        RetryClass::Unknown => tau_proto::ProviderRetryCategory::Unknown,
    }
}

/// Narrows one retry attempt counter without wrapping the protocol field.
pub(super) fn saturating_retry_attempt(attempt: u64) -> u32 {
    u32::try_from(attempt).unwrap_or(u32::MAX)
}

/// Narrows one retry delay without wrapping the protocol field.
pub(super) fn saturating_retry_delay(delay: Duration) -> u32 {
    u32::try_from(delay.as_secs()).unwrap_or(u32::MAX)
}
