//! Typed post-commit reducer state for reactive context recovery.

use tau_proto::ConnectionId;

/// Exact state used to reduce one committed reactive context-recovery terminal.
#[derive(Clone)]
pub(crate) struct CommittedReactiveContextRecovery {
    /// Durable failed inference owner that the transaction must claim.
    pub(super) checkpoint: tau_proto::AgentInferenceDispatchStarted,
    /// Provider connection retained only for live attribution.
    pub(super) source: Option<ConnectionId>,
}
