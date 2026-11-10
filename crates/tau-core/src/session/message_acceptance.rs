//! Retained canonical input identity, independent of transcript cache
//! residency.

use tau_proto::{MessageDelivered, MessageFactId, MessagePublisherId};

use super::AgentTree;
use crate::PersistedAgentEventSeq;

/// Compact evidence reconstructed from the canonical message itself.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct AcceptedMessage {
    /// Versioned typed semantic content digest, excluding observation metadata.
    pub(super) digest: blake3::Hash,
    /// Original canonical occurrence, not the current transcript position.
    pub(super) sequence: PersistedAgentEventSeq,
}

/// Publisher and opaque base-message identity within one recipient journal.
pub(super) type AcceptedMessageKey = (MessagePublisherId, MessageFactId);

impl AgentTree {
    /// Find an opt-in accepted identity even after history-cache eviction.
    ///
    /// `Some((false, _))` means conflicting reuse, not an absent message.
    /// Replay reconstructs this evidence only from surviving canonical
    /// facts.
    pub fn message_acceptance_matches(
        &self,
        message: &MessageDelivered,
    ) -> Option<(bool, PersistedAgentEventSeq)> {
        self.indexes
            .accepted_messages
            .get(&(
                message.publisher_extension_id.clone(),
                message.message_id.clone(),
            ))
            .map(|accepted| {
                (
                    accepted.digest == semantic_digest(message),
                    accepted.sequence,
                )
            })
    }

    /// Fold evidence only for a successfully projectable marked message.
    pub(super) fn record_message_acceptance(
        &mut self,
        message: &MessageDelivered,
        sequence: PersistedAgentEventSeq,
    ) {
        if message.idempotent_acceptance {
            self.indexes
                .accepted_messages
                .entry((
                    message.publisher_extension_id.clone(),
                    message.message_id.clone(),
                ))
                .or_insert_with(|| AcceptedMessage {
                    digest: semantic_digest(message),
                    sequence,
                });
        }
    }
}

/// Explicit versioned field order; transport correlation, marker, timestamp and
/// sequence never enter identity. Opaque CBOR retains its exact typed value,
/// including map entry order, rather than silently normalizing bridge content.
/// The display-only connection ID is deliberately excluded; all preexisting
/// v2 inputs remain unchanged, including canonical sender and incoming notice.
fn semantic_digest(message: &MessageDelivered) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    ciborium::into_writer(
        &(
            "tau.message-acceptance.v2",
            &message.publisher_extension_id,
            &message.agent_id,
            &message.message_id,
            &message.links,
            &message.sender,
            &message.presentation,
            &message.mention_evidence,
            &message.reply_context,
            &message.conversation,
            &message.text,
            &message.extension_data,
            &message.incoming_notice,
        ),
        &mut hasher,
    )
    .expect("typed message content is CBOR encodable");
    hasher.finalize()
}
