//! Short-lived classification and eager execution state for provider terminals.

use tau_proto::ConnectionId;

/// Exhaustive classification at each incrementally typed provider-terminal
/// family boundary.
pub(crate) enum ProviderTerminalPlan {
    /// The terminal is an eligible ordinary-inference context rejection.
    ReactiveContextRecovery(Box<ReactiveContextRecoveryPlan>),
    /// The terminal is governed by the agent's final-status contract.
    FinalStatusGated(FinalStatusGatedPlan),
    /// The terminal is commit-gated by an automatic compaction decision or a
    /// pending side-conversation message wake.
    AutomaticCompactionOrPendingMessageWake(AutomaticCompactionOrPendingMessageWakePlan),
    /// The terminal belongs to another provider-terminal family.
    Other,
}

/// Exact authority needed to execute one reactive context-recovery terminal.
pub(crate) struct ReactiveContextRecoveryPlan {
    /// Durable failed inference owner that the transaction must claim.
    pub(super) checkpoint: tau_proto::AgentInferenceDispatchStarted,
    /// Provider connection retained only for live attribution.
    pub(super) source: Option<ConnectionId>,
}

/// Exact final-status decision retained through eager terminal preparation.
pub(crate) enum FinalStatusGatedPlan {
    /// Queue the captured status reminder after the response commits.
    Challenge {
        /// Validated unresolved status captured with the response.
        challenge: crate::agent::FinalStatusChallenge,
    },
    /// Apply ordinary committed terminal projection after the response commits.
    Accept,
}

/// Exact deferred tool effect for an automatic-compaction-owned or pending
/// message-wake terminal.
pub(crate) struct AutomaticCompactionOrPendingMessageWakePlan {
    /// Tool effect withheld until the response commits.
    pub(super) tool_effect: super::CommittedOutputLengthToolEffect,
}

/// Complete semantic input for classifying an automatic-compaction-owned or
/// pending-message-wake terminal.
pub(crate) struct AutomaticCompactionOrPendingMessageWakeClassification {
    /// Whether final-status ownership already claimed this terminal.
    pub(super) final_status_owned: bool,
    /// Whether the eager automatic-compaction policy owns this terminal.
    pub(super) automatic_compaction_owned: bool,
    /// Whether a pending side-conversation message must wake after this
    /// terminal.
    pub(super) continues_for_pending_message_wake: bool,
    /// Exact deferred tool effect, including its normalized call payload.
    pub(super) tool_effect: super::CommittedOutputLengthToolEffect,
}
