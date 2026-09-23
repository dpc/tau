//! Private configured-message-bridge receiver resolution, not a roster API.

use serde::{Deserialize, Serialize};

use crate::{AgentId, SessionId, ToolCallId};

/// Resolve an ordinary current-session receiving agent without assigning input.
///
/// Only configured peers declaring `MessageBridge` may use this operation.
/// The bridge owns its designation snapshot and must fence stale replies before
/// replacing it. A selected identity is not an external-message
/// acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeReceiverRequest {
    /// Caller correlation, nonempty and at most 128 bytes.
    pub request_id: String,
    /// Exact current session; resolution never crosses or changes sessions.
    pub session_id: SessionId,
    /// Optional immutable creation-role constraint; unknown roles are errors.
    pub role: Option<String>,
    /// Whether to restore, select, lazily ensure, or validate a tool caller.
    pub mode: BridgeReceiverMode,
}

/// Receiver resolution authority requested by a configured bridge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeReceiverMode {
    /// Restore only this saved designation, without loading an absent agent.
    Restore {
        /// Previously saved concrete identity, if any.
        agent_id: Option<AgentId>,
    },
    /// Reuse a valid designation or select the oldest eligible loaded agent.
    Select {
        /// Saved designation wins over oldest-agent selection when eligible.
        preferred_agent_id: Option<AgentId>,
    },
    /// Select as above, or create the explicit role without a bootstrap prompt.
    ///
    /// Send only after admitting actual external input, never at startup.
    Ensure {
        /// Saved designation wins over oldest-agent selection when eligible.
        preferred_agent_id: Option<AgentId>,
    },
    /// Validate the authenticated caller of this bridge's live routed tool.
    ///
    /// Never substitutes a different agent or starts one.
    Register {
        /// Live tool invocation routed to the requesting extension connection.
        tool_call_id: ToolCallId,
    },
}

/// Correlated private resolution response; never a journal fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeReceiverResult {
    /// Exact request correlation.
    pub request_id: String,
    /// Selected identity, retryable absence, or explicit rejection.
    pub outcome: BridgeReceiverOutcome,
}

/// Outcome of receiver resolution without changing bridge designation state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BridgeReceiverOutcome {
    /// Eligible loaded identity; the bridge must persist before designating it.
    Selected {
        /// Concrete current-session agent.
        agent_id: AgentId,
    },
    /// No target now; retained input remains subject to the bridge's bounds.
    Unavailable {
        /// Distinguishes readiness/start-in-progress from ordinary absence.
        reason: BridgeReceiverUnavailable,
    },
    /// Request rejected without changing the bridge's saved designation.
    Error {
        /// Machine-readable rejection category.
        kind: BridgeReceiverErrorKind,
        /// Human-readable diagnostic.
        message: String,
    },
}

/// Retryable receiver resolution absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeReceiverUnavailable {
    /// Session restoration or an eligible agent's creation is still in
    /// progress.
    NotReady,
    /// No eligible loaded identity and no permitted explicit-role creation.
    NoEligibleAgent,
}

/// Receiver request rejection categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeReceiverErrorKind {
    /// Malformed correlation or wrong session.
    InvalidRequest,
    /// Requester is not a configured message bridge.
    Unauthorized,
    /// Configured role is unknown or cannot currently create an agent.
    InvalidRole,
    /// Tool caller is absent, ineligible, or not routed to this requester.
    InvalidCaller,
    /// Ordinary start preparation or lifecycle admission failed.
    CreationFailed,
}

#[cfg(test)]
mod tests;
