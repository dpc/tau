//! Frozen descriptive reply context supplied with one admitted native message.

use serde::{Deserialize, Serialize};

use crate::{MessageFactRef, MessagePublisherId};

/// Optional same-event reply or thread-root snapshot, never a second delivery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageReplyContext {
    /// The native relation, not an inferred immediately preceding message.
    pub relation: MessageReplyRelation,
    /// An existing canonical reference or an opaque descriptive identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<MessageReplyReference>,
    /// Useful supplied author identity, without authentication or admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Supplied parent body or excerpt; absence does not prove deletion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Whether the supplied text is a sender-selected excerpt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub excerpt: bool,
}

/// Native evidence distinguishing an actual reply from a thread-root relation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageReplyRelation {
    /// An actual native reply parent.
    Reply,
    /// A thread root, not necessarily the immediate reply parent.
    ThreadRoot,
}

/// Descriptive context identity, separate from live tool ownership.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "reference", rename_all = "snake_case")]
pub enum MessageReplyReference {
    /// A producer-confirmed existing canonical fact in the same
    /// agent/conversation.
    Canonical(MessageFactRef),
    /// Opaque native relation identity, never synthesized tool ownership.
    Descriptive(String),
}

impl MessageReplyContext {
    /// Omit invalid optional enrichment without rejecting the admitted outer
    /// body.
    pub(crate) fn valid(&self) -> bool {
        self.author
            .as_deref()
            .is_none_or(super::message_fact::valid_display)
            && self.text.as_ref().is_none_or(|text| text.len() <= 2_048)
            && self
                .reference
                .as_ref()
                .is_none_or(|reference| match reference {
                    MessageReplyReference::Canonical(reference) => {
                        MessagePublisherId::parse(reference.publisher_extension_id.as_str()).is_ok()
                            && !reference.message_id.as_str().is_empty()
                            && reference.message_id.as_str().len() <= 256
                    }
                    MessageReplyReference::Descriptive(reference) => {
                        !reference.is_empty() && reference.len() <= 256
                    }
                })
    }
}
