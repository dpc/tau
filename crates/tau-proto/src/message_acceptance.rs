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
    /// Semantic message content; publisher authority comes from the connection.
    pub report: MessageDelivered<RawMessagePublisherId>,
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
    /// This attempt did not accept new input; an older accepted ID stays
    /// accepted.
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
