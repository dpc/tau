//! Shared wait mode and settlement consistency policy.

pub(super) fn wait_mode_allows_outcome(
    mode: &tau_proto::ToolWaitMode,
    settlement: &tau_proto::AgentToolWaitSettled,
) -> bool {
    use tau_proto::{
        ToolWaitMode as Mode, ToolWaitOutcome as Outcome, WaitRejectionReason as Reject,
    };
    let registered = settlement.registration.is_some();
    match (mode, &settlement.outcome) {
        (
            Mode::Exact { target },
            Outcome::CompletionDelivered {
                source_call,
                envelope,
                ..
            },
        ) => target == source_call && *envelope == tau_proto::ToolOutputEnvelope::Identity,
        (Mode::ExactAll { targets }, Outcome::CompletionsDelivered { sources }) => {
            targets.len() == sources.len()
                && targets.iter().zip(sources).all(|(target, source)| {
                    target == &source.source_call
                        && source.envelope == tau_proto::ToolOutputEnvelope::Identity
                })
        }
        (
            Mode::NextBackground,
            Outcome::CompletionDelivered {
                source_phase,
                envelope,
                ..
            },
        ) => {
            *source_phase == tau_proto::ToolSourcePhase::Background
                && *envelope == tau_proto::ToolOutputEnvelope::OriginalToolCallIdHeader
        }
        (
            Mode::Exact { .. } | Mode::ExactAll { .. } | Mode::NextBackground,
            Outcome::InterruptedByActivation { .. },
        )
        | (Mode::ActivatingInput { .. }, Outcome::InputAvailable { .. }) => true,
        (Mode::ActivatingInput { .. }, Outcome::TimedOut)
        | (
            Mode::Exact { .. }
            | Mode::ExactAll { .. }
            | Mode::NextBackground
            | Mode::ActivatingInput { .. },
            Outcome::Cancelled | Outcome::LifecycleAborted,
        ) => registered,
        (
            Mode::Exact { .. } | Mode::ExactAll { .. },
            Outcome::Rejected {
                reason:
                    Reject::DuplicateExactWait
                    | Reject::TargetReturnedForegroundBeforeWait
                    | Reject::ResultAlreadyConsumed,
            },
        )
        | (
            Mode::NextBackground,
            Outcome::Rejected {
                reason: Reject::DuplicateAnyWait,
            },
        )
        | (
            Mode::ActivatingInput { .. },
            Outcome::Rejected {
                reason: Reject::DuplicateInputWait,
            },
        )
        | (
            Mode::ExactUnresolved,
            Outcome::Rejected {
                reason: Reject::UnknownTarget,
            },
        )
        | (
            Mode::ExactAllUnresolved,
            Outcome::Rejected {
                reason: Reject::UnknownTarget,
            },
        )
        | (
            Mode::InvalidArguments,
            Outcome::Rejected {
                reason: Reject::InvalidArguments,
            },
        ) => !registered,
        (
            Mode::NextBackground,
            Outcome::Rejected {
                reason: Reject::NoBackgroundCandidate,
            },
        ) => true,
        _ => false,
    }
}
