//! Short-lived classification and eager execution state for provider terminals.

use tau_proto::ConnectionId;

/// Exhaustive classification at the reactive context-recovery family boundary.
pub(crate) enum ProviderTerminalPlan {
    /// The terminal is an eligible ordinary-inference context rejection.
    ReactiveContextRecovery(Box<ReactiveContextRecoveryPlan>),
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
