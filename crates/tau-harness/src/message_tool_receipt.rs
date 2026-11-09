//! Payload-free message acknowledgements shared by immediate and deferred
//! delivery.

use tau_proto::{AgentMessageId, CborValue};

/// Builds the receipt for a message accepted by a live recipient.
pub fn message_committed_receipt(message_id: &AgentMessageId) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("msg_id".to_owned()),
        CborValue::Text(message_id.to_string()),
    )])
}
