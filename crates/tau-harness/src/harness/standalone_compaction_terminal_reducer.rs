//! Typed reducer state for standalone-compaction provider terminals.

use tau_proto::{ConnectionId, ProviderResponseFinished};

use super::provider_terminal_plan::StandaloneCompactionTerminalPlan;

/// Exact eager reducer input for one classified standalone-compaction terminal.
pub(crate) struct EagerStandaloneCompactionTerminal<'a> {
    /// Conversation whose active standalone transaction owns the terminal.
    pub(super) cid: &'a tau_proto::AgentId,
    /// Typed accepted or rejected terminal decision.
    pub(super) plan: StandaloneCompactionTerminalPlan,
    /// Fully prepared canonical provider terminal.
    pub(super) response: &'a ProviderResponseFinished,
    /// Provider connection retained for derived-fact attribution.
    pub(super) source: Option<&'a ConnectionId>,
}

/// Exact state used to derive a standalone context failure after its canonical
/// provider rejection commits.
#[derive(Clone)]
pub(crate) struct CommittedStandaloneContextRejection {
    /// Exact canonical response used to derive the post-commit failure.
    pub(super) response: Box<ProviderResponseFinished>,
    /// Provider connection retained for derived-failure attribution.
    pub(super) source: Option<ConnectionId>,
}
