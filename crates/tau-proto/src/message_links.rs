//! Optional publisher-scoped links for bridge-owned exact-message retrieval.

use serde::{Deserialize, Serialize};

use crate::MessageFactId;

/// Read-only message identities in the containing fact's publisher namespace.
///
/// A bridge's `get_message` tool accepts these IDs and the projected
/// `message_ref` as its `message_id` argument. Links do not grant reply, send,
/// or reaction authority. Omit links the bridge cannot establish reliably.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageLinks {
    /// Root of this thread or conversation branch, not an arbitrary channel
    /// post.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<MessageFactId>,
    /// Immediately preceding message in this same thread, topic, or DM.
    ///
    /// This is not merely the last message delivered to Tau or a neighboring
    /// message elsewhere in the surrounding channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_id: Option<MessageFactId>,
    /// Immediate native reply target, independently of the thread root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_id: Option<MessageFactId>,
}
