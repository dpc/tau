//! Opt-in retained-history idempotency for configured message bridges.

use serde::{Deserialize, Serialize};

use crate::{MessageDelivered, RawMessagePublisherId, SessionId};

/// Submit one immutable incoming message without creating or loading a
/// recipient.
///
/// This control-plane request publishes only the canonical message fact, not a
/// `MessageDeliveredReported` event. Acceptance never waits for filesystem I/O.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageAcceptanceRequest {
    /// Attempt correlation, nonempty and at most 128 UTF-8 bytes.
    pub request_id: String,
    /// Exact currently attached session.
    pub session_id: SessionId,
    /// Whether this attempt may accept absent input. This control does not
    /// participate in the immutable message identity.
    #[serde(default)]
    pub mode: MessageAcceptanceMode,
    /// Semantic message content; publisher authority comes from the connection.
    pub report: MessageDelivered<RawMessagePublisherId>,
}

/// Policy applied atomically with retained identity lookup at final admission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageAcceptanceMode {
    /// Accept an absent message, or return the matching retained acceptance.
    #[default]
    AcceptNew,
    /// Return matching retained acceptance, but never append absent input.
    ///
    /// Bridges can use this after declining new traffic without falsely
    /// refusing a retry whose earlier acceptance response was lost.
    ExistingOnly,
}

/// Directed reply to the exact requesting connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageAcceptanceResult {
    /// Original attempt correlation; callers retain the corresponding request.
    pub request_id: String,
    /// Ordinary semantic acceptance, definite refusal, or uncertain completion.
    pub outcome: MessageAcceptanceOutcome,
}

/// Acceptance is neither filesystem durability nor execution acknowledgement.
///
/// A crash may lose accepted input. A retry is suppressed only while its
/// canonical fact survives; effects performed before a lost tail may repeat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MessageAcceptanceOutcome {
    /// The loaded recipient owns this input under ordinary publication
    /// semantics.
    Accepted {
        /// An identical retained fact already owned the input; no new wake
        /// occurs.
        duplicate: bool,
    },
    /// Explicit refusal of this RPC, not proof that its ID was never accepted.
    /// An older accepted identity is not undone by policy or conflict
    /// rejection.
    Rejected {
        /// Bounded diagnostic, never the submitted body.
        reason: String,
    },
    /// Completion could not be established. Retry the identical semantic
    /// request.
    Unknown {
        /// Bounded diagnostic, never a definite refusal.
        reason: String,
    },
}

#[cfg(test)]
mod tests;
