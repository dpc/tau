//! Typed eager reducer state for ordinary provider tool-call terminals.

use tau_proto::ConnectionId;

/// Exact state used to dispatch one normalized provider tool-call round.
pub(crate) struct EagerToolCallTerminal {
    /// Exact normalized call aggregate prepared before response publication.
    pub(super) normalized_tool_calls: super::NormalizedFinishedToolCalls,
    /// Provider connection retained for tool-result attribution.
    pub(super) source: Option<ConnectionId>,
}
