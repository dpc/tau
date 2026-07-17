//! Transport-neutral extension-published message fact schema.

#[cfg(test)]
#[path = "message_fact/tests.rs"]
mod tests;

use serde::{Deserialize, Serialize};

use crate::MessageExtensionData;

/// Publisher-scoped opaque identifier for one base message fact.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageFactId(
    /// Opaque publisher-defined identifier bytes.
    String,
);

impl MessageFactId {
    /// Construct an opaque message fact identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the opaque identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Raw wire-decodable identifier for the extension that published a fact.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessagePublisherId(
    /// Raw claimed publisher name retained for post-commit validation.
    String,
);

impl MessagePublisherId {
    /// Construct a raw publisher identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the raw publisher identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Return whether this identifier follows the configured publisher grammar.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        crate::valid_extension_name(&self.0)
    }
}

/// Opaque reference to a publisher-scoped base message fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageFactRef {
    /// Extension publisher namespace containing the target identifier.
    pub publisher_extension_id: MessagePublisherId,
    /// Opaque target message identifier inside the publisher namespace.
    pub message_id: MessageFactId,
}

/// One external participant described by a message publisher.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageParty {
    /// Opaque stable identifier in the publisher's identity domain.
    pub stable_id: String,
    /// Optional presentation-only display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Descriptive conversation provenance supplied by a message publisher.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageConversation {
    /// Opaque stable identifier in the publisher's conversation domain.
    pub stable_id: String,
    /// Optional presentation-only conversation label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Raw claimed Tau transcript target retained even when it cannot parse.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageAgentTarget(
    /// Raw publisher-supplied target bytes.
    String,
);

impl MessageAgentTarget {
    /// Construct a raw claimed agent target.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the raw claimed target.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Immutable fact reporting an externally delivered text message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageDelivered {
    /// Harness-stamped configured extension publisher identifier.
    pub publisher_extension_id: MessagePublisherId,
    /// Raw claimed Tau transcript target.
    pub agent_id: MessageAgentTarget,
    /// Publisher-scoped opaque base-message identifier.
    pub message_id: MessageFactId,
    /// External sender described by the publisher.
    pub sender: MessageParty,
    /// Optional descriptive conversation provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<MessageConversation>,
    /// Untrusted delivered text.
    pub text: String,
    /// Required bounded extension-private value.
    pub extension_data: MessageExtensionData,
}

impl MessageDelivered {
    /// Construct a delivered-message fact with CBOR null extension data.
    pub fn new(
        publisher_extension_id: MessagePublisherId,
        agent_id: MessageAgentTarget,
        message_id: MessageFactId,
        sender: MessageParty,
        conversation: Option<MessageConversation>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            publisher_extension_id,
            agent_id,
            message_id,
            sender,
            conversation,
            text: text.into(),
            extension_data: MessageExtensionData::default(),
        }
    }
}

/// Immutable fact reporting replacement text for a referenced message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageEdited {
    /// Harness-stamped configured extension publisher identifier.
    pub publisher_extension_id: MessagePublisherId,
    /// Raw claimed Tau transcript target.
    pub agent_id: MessageAgentTarget,
    /// Opaque referenced base message.
    pub target: MessageFactRef,
    /// Optional external actor described by the publisher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<MessageParty>,
    /// Optional descriptive conversation provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<MessageConversation>,
    /// Untrusted replacement text.
    pub text: String,
    /// Required bounded extension-private value.
    pub extension_data: MessageExtensionData,
}

impl MessageEdited {
    /// Construct an edited-message fact with CBOR null extension data.
    pub fn new(
        publisher_extension_id: MessagePublisherId,
        agent_id: MessageAgentTarget,
        target: MessageFactRef,
        actor: Option<MessageParty>,
        conversation: Option<MessageConversation>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            publisher_extension_id,
            agent_id,
            target,
            actor,
            conversation,
            text: text.into(),
            extension_data: MessageExtensionData::default(),
        }
    }
}

/// Immutable fact reporting deletion of a referenced message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageDeleted {
    /// Harness-stamped configured extension publisher identifier.
    pub publisher_extension_id: MessagePublisherId,
    /// Raw claimed Tau transcript target.
    pub agent_id: MessageAgentTarget,
    /// Opaque referenced base message.
    pub target: MessageFactRef,
    /// Optional external actor described by the publisher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<MessageParty>,
    /// Optional descriptive conversation provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<MessageConversation>,
    /// Required bounded extension-private value.
    pub extension_data: MessageExtensionData,
}

impl MessageDeleted {
    /// Construct a deleted-message fact with CBOR null extension data.
    pub fn new(
        publisher_extension_id: MessagePublisherId,
        agent_id: MessageAgentTarget,
        target: MessageFactRef,
        actor: Option<MessageParty>,
        conversation: Option<MessageConversation>,
    ) -> Self {
        Self {
            publisher_extension_id,
            agent_id,
            target,
            actor,
            conversation,
            extension_data: MessageExtensionData::default(),
        }
    }
}

/// Immutable fact reporting a reaction added to a referenced message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageReactionAdded {
    /// Harness-stamped configured extension publisher identifier.
    pub publisher_extension_id: MessagePublisherId,
    /// Raw claimed Tau transcript target.
    pub agent_id: MessageAgentTarget,
    /// Opaque referenced base message.
    pub target: MessageFactRef,
    /// Optional external actor described by the publisher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<MessageParty>,
    /// Optional descriptive conversation provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<MessageConversation>,
    /// Untrusted publisher-defined reaction value.
    pub reaction: String,
    /// Required bounded extension-private value.
    pub extension_data: MessageExtensionData,
}

impl MessageReactionAdded {
    /// Construct a reaction-added fact with CBOR null extension data.
    pub fn new(
        publisher_extension_id: MessagePublisherId,
        agent_id: MessageAgentTarget,
        target: MessageFactRef,
        actor: Option<MessageParty>,
        conversation: Option<MessageConversation>,
        reaction: impl Into<String>,
    ) -> Self {
        Self {
            publisher_extension_id,
            agent_id,
            target,
            actor,
            conversation,
            reaction: reaction.into(),
            extension_data: MessageExtensionData::default(),
        }
    }
}

/// Immutable fact reporting a reaction removed from a referenced message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageReactionRemoved {
    /// Harness-stamped configured extension publisher identifier.
    pub publisher_extension_id: MessagePublisherId,
    /// Raw claimed Tau transcript target.
    pub agent_id: MessageAgentTarget,
    /// Opaque referenced base message.
    pub target: MessageFactRef,
    /// Optional external actor described by the publisher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<MessageParty>,
    /// Optional descriptive conversation provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<MessageConversation>,
    /// Untrusted publisher-defined reaction value.
    pub reaction: String,
    /// Required bounded extension-private value.
    pub extension_data: MessageExtensionData,
}

impl MessageReactionRemoved {
    /// Construct a reaction-removed fact with CBOR null extension data.
    pub fn new(
        publisher_extension_id: MessagePublisherId,
        agent_id: MessageAgentTarget,
        target: MessageFactRef,
        actor: Option<MessageParty>,
        conversation: Option<MessageConversation>,
        reaction: impl Into<String>,
    ) -> Self {
        Self {
            publisher_extension_id,
            agent_id,
            target,
            actor,
            conversation,
            reaction: reaction.into(),
            extension_data: MessageExtensionData::default(),
        }
    }
}

/// Immutable fact reporting publisher-defined remote send success.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageSent {
    /// Harness-stamped configured extension publisher identifier.
    pub publisher_extension_id: MessagePublisherId,
    /// Raw claimed Tau transcript target.
    pub agent_id: MessageAgentTarget,
    /// Publisher-scoped opaque base-message identifier.
    pub message_id: MessageFactId,
    /// Optional external recipient described by the publisher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<MessageParty>,
    /// Optional descriptive conversation provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<MessageConversation>,
    /// Untrusted sent text.
    pub text: String,
    /// Required bounded extension-private value.
    pub extension_data: MessageExtensionData,
}

impl MessageSent {
    /// Construct a sent-message fact with CBOR null extension data.
    pub fn new(
        publisher_extension_id: MessagePublisherId,
        agent_id: MessageAgentTarget,
        message_id: MessageFactId,
        recipient: Option<MessageParty>,
        conversation: Option<MessageConversation>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            publisher_extension_id,
            agent_id,
            message_id,
            recipient,
            conversation,
            text: text.into(),
            extension_data: MessageExtensionData::default(),
        }
    }
}
