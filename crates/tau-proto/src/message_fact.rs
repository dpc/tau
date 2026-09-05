//! Shared transport-neutral payloads for bridge reports and canonical message
//! facts.
//!
//! In `message.*_reported`, `publisher_extension_id` is an untrusted peer
//! claim. In canonical `message.*`, the harness stamps the authenticated
//! configured extension identity.

#[cfg(test)]
#[path = "message_fact/tests.rs"]
mod tests;

use serde::{Deserialize, Serialize};

use crate::{
    AgentId, ContentPart, ContextRole, Event, MESSAGE_PAYLOAD_ENVELOPE, MessageExtensionData,
    MessageItem, visible_escape_metadata,
};

const MESSAGE_ID_MAX_BYTES: usize = 256;
const STABLE_ID_MAX_BYTES: usize = 4_096;
const DISPLAY_MAX_BYTES: usize = 256;
const DISPLAY_MAX_SCALARS: usize = 80;
const REACTION_MAX_BYTES: usize = 128;
const REACTION_MAX_SCALARS: usize = 64;
const MESSAGE_TEXT_MAX_BYTES: usize = 131_072;

/// Deterministic reason a committed message fact has no model projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageProjectionFailure {
    /// The claimed transcript target is not a valid agent identifier.
    InvalidTarget,
    /// A delivered/sent base-message identifier is invalid.
    InvalidMessageId,
    /// An operation target publisher or message identifier is invalid.
    InvalidReference,
    /// A sender, actor, or recipient identifier/display is invalid.
    InvalidParty,
    /// Conversation identifier/display metadata is invalid.
    InvalidConversation,
    /// A reaction is empty or exceeds its universal limit.
    InvalidReaction,
    /// A text-bearing fact contains empty text.
    EmptyText,
    /// A text-bearing fact exceeds the universal text limit.
    TextTooLarge,
}

impl MessageProjectionFailure {
    /// Return the stable categorical wire/UI label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidTarget => "invalid_target",
            Self::InvalidMessageId => "invalid_message_id",
            Self::InvalidReference => "invalid_reference",
            Self::InvalidParty => "invalid_party",
            Self::InvalidConversation => "invalid_conversation",
            Self::InvalidReaction => "invalid_reaction",
            Self::EmptyText => "empty_text",
            Self::TextTooLarge => "text_too_large",
        }
    }
}

/// Successful generic model projection of one committed message fact.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageFactProjection {
    /// Ordinary context message carrying the escaped external `message`
    /// boundary.
    pub item: MessageItem,
    /// Whether a live commit requests one agent activation.
    pub activates_model: bool,
}

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

crate::validated_string_newtype!(
    /// Harness-authenticated canonical publisher identity.
    ///
    /// Values contain 1 through 128 bytes of ASCII letters, digits, `_`, or
    /// `-`. Construction and deserialization validate this grammar.
    MessagePublisherId,
    MessagePublisherIdParseError,
    "message publisher id",
    128
);

impl MessagePublisherId {
    /// Preserve a configured extension name's identical validated syntax as a
    /// canonical publisher identifier.
    ///
    /// This conversion does not grant authority; callers must authenticate the
    /// extension identity before using the result as canonical provenance.
    #[must_use]
    pub fn from_extension_name(name: &crate::ExtensionName) -> Self {
        Self(name.to_string())
    }
}

/// Lossless wire-decodable publisher claim supplied by an untrusted peer.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RawMessagePublisherId(
    /// Untrusted publisher claim retained exactly as decoded.
    String,
);

impl RawMessagePublisherId {
    /// Construct an opaque publisher claim without applying canonical grammar.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the opaque publisher claim.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Opaque reference to a publisher-scoped base message fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageFactRef {
    /// Opaque claimed publisher namespace containing the target identifier.
    pub publisher_extension_id: RawMessagePublisherId,
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
    /// Optional publisher-established authentication and admission outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_auth: Option<MessageSenderAuth>,
}

/// Publisher-established sender authentication and admission outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSenderAuth {
    /// The transport verified the sender and an operator allowlist admitted it.
    VerifiedAllowlisted,
    /// The transport verified the sender and conversation policy admitted it.
    VerifiedConversationAuthorized,
    /// Configured room membership admitted the sender without individual proof.
    TrustedMembership,
}

/// Descriptive conversation provenance supplied by a message publisher.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageConversation {
    /// Opaque stable identifier in the publisher's conversation domain.
    pub stable_id: String,
    /// Optional presentation-only conversation label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional configured human-readable model-facing conversation alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
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

/// Shared report/canonical payload describing an externally delivered text
/// message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageDelivered<Publisher = MessagePublisherId> {
    /// Raw report claim or harness-stamped canonical publisher identity.
    pub publisher_extension_id: Publisher,
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

impl<Publisher> MessageDelivered<Publisher> {
    /// Construct a delivered-message payload with CBOR null extension data.
    pub fn new(
        publisher_extension_id: Publisher,
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

    /// Replace the publisher field while preserving the report payload exactly.
    pub fn with_publisher<Canonical>(
        self,
        publisher_extension_id: Canonical,
    ) -> MessageDelivered<Canonical> {
        MessageDelivered {
            publisher_extension_id,
            agent_id: self.agent_id,
            message_id: self.message_id,
            sender: self.sender,
            conversation: self.conversation,
            text: self.text,
            extension_data: self.extension_data,
        }
    }
}

/// Shared report/canonical payload describing replacement text for a referenced
/// message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageEdited<Publisher = MessagePublisherId> {
    /// Untrusted in a report; harness-stamped in a canonical fact.
    pub publisher_extension_id: Publisher,
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

impl<Publisher> MessageEdited<Publisher> {
    /// Construct an edited-message payload with CBOR null extension data.
    pub fn new(
        publisher_extension_id: Publisher,
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

    /// Replace the publisher field while preserving the report payload exactly.
    pub fn with_publisher<Canonical>(
        self,
        publisher_extension_id: Canonical,
    ) -> MessageEdited<Canonical> {
        MessageEdited {
            publisher_extension_id,
            agent_id: self.agent_id,
            target: self.target,
            actor: self.actor,
            conversation: self.conversation,
            text: self.text,
            extension_data: self.extension_data,
        }
    }
}

/// Shared report/canonical payload describing deletion of a referenced message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageDeleted<Publisher = MessagePublisherId> {
    /// Untrusted in a report; harness-stamped in a canonical fact.
    pub publisher_extension_id: Publisher,
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

impl<Publisher> MessageDeleted<Publisher> {
    /// Construct a deleted-message payload with CBOR null extension data.
    pub fn new(
        publisher_extension_id: Publisher,
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

    /// Replace the publisher field while preserving the report payload exactly.
    pub fn with_publisher<Canonical>(
        self,
        publisher_extension_id: Canonical,
    ) -> MessageDeleted<Canonical> {
        MessageDeleted {
            publisher_extension_id,
            agent_id: self.agent_id,
            target: self.target,
            actor: self.actor,
            conversation: self.conversation,
            extension_data: self.extension_data,
        }
    }
}

/// Shared report/canonical payload describing a reaction added to a referenced
/// message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageReactionAdded<Publisher = MessagePublisherId> {
    /// Untrusted in a report; harness-stamped in a canonical fact.
    pub publisher_extension_id: Publisher,
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

impl<Publisher> MessageReactionAdded<Publisher> {
    /// Construct a reaction-added payload with CBOR null extension data.
    pub fn new(
        publisher_extension_id: Publisher,
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

    /// Replace the publisher field while preserving the report payload exactly.
    pub fn with_publisher<Canonical>(
        self,
        publisher_extension_id: Canonical,
    ) -> MessageReactionAdded<Canonical> {
        MessageReactionAdded {
            publisher_extension_id,
            agent_id: self.agent_id,
            target: self.target,
            actor: self.actor,
            conversation: self.conversation,
            reaction: self.reaction,
            extension_data: self.extension_data,
        }
    }
}

/// Shared report/canonical payload describing a reaction removed from a
/// referenced message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageReactionRemoved<Publisher = MessagePublisherId> {
    /// Untrusted in a report; harness-stamped in a canonical fact.
    pub publisher_extension_id: Publisher,
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

impl<Publisher> MessageReactionRemoved<Publisher> {
    /// Construct a reaction-removed payload with CBOR null extension data.
    pub fn new(
        publisher_extension_id: Publisher,
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

    /// Replace the publisher field while preserving the report payload exactly.
    pub fn with_publisher<Canonical>(
        self,
        publisher_extension_id: Canonical,
    ) -> MessageReactionRemoved<Canonical> {
        MessageReactionRemoved {
            publisher_extension_id,
            agent_id: self.agent_id,
            target: self.target,
            actor: self.actor,
            conversation: self.conversation,
            reaction: self.reaction,
            extension_data: self.extension_data,
        }
    }
}

/// Shared report/canonical payload describing publisher-defined remote send
/// success.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageSent<Publisher = MessagePublisherId> {
    /// Untrusted in a report; harness-stamped in a canonical fact.
    pub publisher_extension_id: Publisher,
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

impl<Publisher> MessageSent<Publisher> {
    /// Construct a sent-message payload with CBOR null extension data.
    pub fn new(
        publisher_extension_id: Publisher,
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

    /// Replace the publisher field while preserving the report payload exactly.
    pub fn with_publisher<Canonical>(
        self,
        publisher_extension_id: Canonical,
    ) -> MessageSent<Canonical> {
        MessageSent {
            publisher_extension_id,
            agent_id: self.agent_id,
            message_id: self.message_id,
            recipient: self.recipient,
            conversation: self.conversation,
            text: self.text,
            extension_data: self.extension_data,
        }
    }
}

/// Validate and render one message fact as an ordinary provider context
/// message.
///
/// Non-message events return `None`; committed but universally invalid facts
/// return their deterministic failure reason without echoing untrusted
/// payloads.
#[must_use]
pub fn project_message_fact(
    event: &Event,
) -> Option<Result<MessageFactProjection, MessageProjectionFailure>> {
    let view = MessageFactView::from_event(event)?;
    if AgentId::parse(view.agent_id().as_str()).is_err() {
        return Some(Err(MessageProjectionFailure::InvalidTarget));
    }
    if let Some(message_id) = view.message_id()
        && !valid_opaque_id(message_id.as_str())
    {
        return Some(Err(MessageProjectionFailure::InvalidMessageId));
    }
    if let Some(reference) = view.reference()
        && (MessagePublisherId::parse(reference.publisher_extension_id.as_str()).is_err()
            || !valid_opaque_id(reference.message_id.as_str()))
    {
        return Some(Err(MessageProjectionFailure::InvalidReference));
    }
    if view.party().is_some_and(|party| !valid_party(party)) {
        return Some(Err(MessageProjectionFailure::InvalidParty));
    }
    if view
        .conversation()
        .is_some_and(|conversation| !valid_conversation(conversation))
    {
        return Some(Err(MessageProjectionFailure::InvalidConversation));
    }
    if let Some(reaction) = view.reaction()
        && (reaction.is_empty()
            || reaction.len() > REACTION_MAX_BYTES
            || reaction.chars().count() > REACTION_MAX_SCALARS)
    {
        return Some(Err(MessageProjectionFailure::InvalidReaction));
    }
    if let Some(text) = view.text() {
        if text.is_empty() {
            return Some(Err(MessageProjectionFailure::EmptyText));
        }
        if text.len() > MESSAGE_TEXT_MAX_BYTES {
            return Some(Err(MessageProjectionFailure::TextTooLarge));
        }
    }

    Some(Ok(MessageFactProjection {
        item: MessageItem {
            role: view.role(),
            content: vec![ContentPart::Text {
                text: render_message_fact(&view),
            }],
            phase: None,
            responses_raw_json: None,
        },
        activates_model: view.activates_model(),
    }))
}

/// Exhaustive borrowed view of one concrete message fact.
enum MessageFactView<'a> {
    /// Delivered base message.
    Delivered(&'a MessageDelivered),
    /// Edited referenced message.
    Edited(&'a MessageEdited),
    /// Deleted referenced message.
    Deleted(&'a MessageDeleted),
    /// Reaction added to a referenced message.
    ReactionAdded(&'a MessageReactionAdded),
    /// Reaction removed from a referenced message.
    ReactionRemoved(&'a MessageReactionRemoved),
    /// Successfully sent base message.
    Sent(&'a MessageSent),
}

impl<'a> MessageFactView<'a> {
    /// Borrow a typed view from any of the six message event variants.
    fn from_event(event: &'a Event) -> Option<Self> {
        match event {
            Event::MessageDelivered(fact) => Some(Self::Delivered(fact)),
            Event::MessageEdited(fact) => Some(Self::Edited(fact)),
            Event::MessageDeleted(fact) => Some(Self::Deleted(fact)),
            Event::MessageReactionAdded(fact) => Some(Self::ReactionAdded(fact)),
            Event::MessageReactionRemoved(fact) => Some(Self::ReactionRemoved(fact)),
            Event::MessageSent(fact) => Some(Self::Sent(fact)),
            _ => None,
        }
    }

    /// Return the stable model-facing occurrence discriminator.
    fn event_name(&self) -> &'static str {
        match self {
            Self::Delivered(_) => "created",
            Self::Edited(_) => "edited",
            Self::Deleted(_) => "deleted",
            Self::ReactionAdded(_) => "reaction_added",
            Self::ReactionRemoved(_) => "reaction_removed",
            Self::Sent(_) => "sent",
        }
    }

    /// Borrow the authenticated publisher namespace.
    fn publisher(&self) -> &MessagePublisherId {
        match self {
            Self::Delivered(fact) => &fact.publisher_extension_id,
            Self::Edited(fact) => &fact.publisher_extension_id,
            Self::Deleted(fact) => &fact.publisher_extension_id,
            Self::ReactionAdded(fact) => &fact.publisher_extension_id,
            Self::ReactionRemoved(fact) => &fact.publisher_extension_id,
            Self::Sent(fact) => &fact.publisher_extension_id,
        }
    }

    /// Borrow the raw transcript target.
    fn agent_id(&self) -> &MessageAgentTarget {
        match self {
            Self::Delivered(fact) => &fact.agent_id,
            Self::Edited(fact) => &fact.agent_id,
            Self::Deleted(fact) => &fact.agent_id,
            Self::ReactionAdded(fact) => &fact.agent_id,
            Self::ReactionRemoved(fact) => &fact.agent_id,
            Self::Sent(fact) => &fact.agent_id,
        }
    }

    /// Borrow a base-message identifier when this is a base fact.
    fn message_id(&self) -> Option<&MessageFactId> {
        match self {
            Self::Delivered(fact) => Some(&fact.message_id),
            Self::Sent(fact) => Some(&fact.message_id),
            Self::Edited(_)
            | Self::Deleted(_)
            | Self::ReactionAdded(_)
            | Self::ReactionRemoved(_) => None,
        }
    }

    /// Borrow the opaque operation target when this is a reference fact.
    fn reference(&self) -> Option<&MessageFactRef> {
        match self {
            Self::Edited(fact) => Some(&fact.target),
            Self::Deleted(fact) => Some(&fact.target),
            Self::ReactionAdded(fact) => Some(&fact.target),
            Self::ReactionRemoved(fact) => Some(&fact.target),
            Self::Delivered(_) | Self::Sent(_) => None,
        }
    }

    /// Borrow the event-specific external party.
    fn party(&self) -> Option<&MessageParty> {
        match self {
            Self::Delivered(fact) => Some(&fact.sender),
            Self::Edited(fact) => fact.actor.as_ref(),
            Self::Deleted(fact) => fact.actor.as_ref(),
            Self::ReactionAdded(fact) => fact.actor.as_ref(),
            Self::ReactionRemoved(fact) => fact.actor.as_ref(),
            Self::Sent(fact) => fact.recipient.as_ref(),
        }
    }

    /// Borrow optional descriptive conversation metadata.
    fn conversation(&self) -> Option<&MessageConversation> {
        match self {
            Self::Delivered(fact) => fact.conversation.as_ref(),
            Self::Edited(fact) => fact.conversation.as_ref(),
            Self::Deleted(fact) => fact.conversation.as_ref(),
            Self::ReactionAdded(fact) => fact.conversation.as_ref(),
            Self::ReactionRemoved(fact) => fact.conversation.as_ref(),
            Self::Sent(fact) => fact.conversation.as_ref(),
        }
    }

    /// Borrow the reaction for reaction facts.
    fn reaction(&self) -> Option<&str> {
        match self {
            Self::ReactionAdded(fact) => Some(&fact.reaction),
            Self::ReactionRemoved(fact) => Some(&fact.reaction),
            Self::Delivered(_) | Self::Edited(_) | Self::Deleted(_) | Self::Sent(_) => None,
        }
    }

    /// Borrow the body for text-bearing facts.
    fn text(&self) -> Option<&str> {
        match self {
            Self::Delivered(fact) => Some(&fact.text),
            Self::Edited(fact) => Some(&fact.text),
            Self::Sent(fact) => Some(&fact.text),
            Self::Deleted(_) | Self::ReactionAdded(_) | Self::ReactionRemoved(_) => None,
        }
    }

    /// Derive the provider context role from the concrete fact type.
    fn role(&self) -> ContextRole {
        match self {
            Self::Sent(_) => ContextRole::Assistant,
            Self::Delivered(_)
            | Self::Edited(_)
            | Self::Deleted(_)
            | Self::ReactionAdded(_)
            | Self::ReactionRemoved(_) => ContextRole::User,
        }
    }

    /// Return whether a live fact requests model activation.
    fn activates_model(&self) -> bool {
        !matches!(self, Self::Sent(_))
    }
}

/// Validate a universal opaque stable identifier.
fn valid_opaque_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MESSAGE_ID_MAX_BYTES
}

/// Validate party stable/display fields.
fn valid_party(party: &MessageParty) -> bool {
    !party.stable_id.is_empty()
        && party.stable_id.len() <= STABLE_ID_MAX_BYTES
        && party.display_name.as_deref().is_none_or(valid_display)
}

/// Validate conversation stable/display fields.
fn valid_conversation(conversation: &MessageConversation) -> bool {
    !conversation.stable_id.is_empty()
        && conversation.stable_id.len() <= STABLE_ID_MAX_BYTES
        && conversation
            .display_name
            .as_deref()
            .is_none_or(valid_display)
        && conversation.alias.as_deref().is_none_or(valid_display)
}

/// Validate one optional presentation label.
fn valid_display(value: &str) -> bool {
    value.len() <= DISPLAY_MAX_BYTES && value.chars().count() <= DISPLAY_MAX_SCALARS
}

/// Render one validated fact with centralized visible Unicode and XML escaping.
fn render_message_fact(view: &MessageFactView<'_>) -> String {
    // Keep this common transport-neutral projection aligned with
    // SPEC-external-message-reports-and-facts.
    let mut output = format!(
        "<message event=\"{}\" publisher=\"{}\"",
        view.event_name(),
        xml_escape(view.publisher().as_str())
    );
    if let Some(message_id) = view.message_id() {
        push_attribute(&mut output, "message_ref", message_id.as_str());
    }
    if let Some(reference) = view.reference() {
        push_attribute(&mut output, "message_ref", reference.message_id.as_str());
    }
    if let Some(party) = view.party() {
        let party_label = if matches!(view, MessageFactView::Sent(_)) {
            "recipient"
        } else {
            "sender"
        };
        push_attribute(&mut output, &format!("{party_label}_ref"), &party.stable_id);
        if let Some(display) = &party.display_name {
            push_attribute(&mut output, &format!("{party_label}_display"), display);
        }
        if !matches!(view, MessageFactView::Sent(_))
            && let Some(sender_auth) = party.sender_auth
        {
            push_attribute(&mut output, "sender_auth", sender_auth.as_str());
        }
    }
    if let Some(alias) = view
        .conversation()
        .and_then(|conversation| conversation.alias.as_deref())
    {
        push_attribute(&mut output, "conversation", alias);
    }
    if let Some(reaction) = view.reaction() {
        push_attribute(&mut output, "reaction", reaction);
    }
    match view.text() {
        Some(text) => {
            if !matches!(view, MessageFactView::Sent(_)) {
                push_attribute(&mut output, "content_trust", "external");
            }
            output.push('>');
            let visible_body = visible_escape_metadata(text);
            output.push_str(&MESSAGE_PAYLOAD_ENVELOPE.escape_body(&visible_body));
            output.push_str(MESSAGE_PAYLOAD_ENVELOPE.exact_close);
        }
        None => output.push_str("/>"),
    }
    output
}

impl MessageSenderAuth {
    /// Return the stable model-facing authentication label.
    const fn as_str(self) -> &'static str {
        match self {
            Self::VerifiedAllowlisted => "verified_allowlisted",
            Self::VerifiedConversationAuthorized => "verified_conversation_authorized",
            Self::TrustedMembership => "trusted_membership",
        }
    }
}

/// Append one escaped XML-like attribute.
fn push_attribute(output: &mut String, name: &str, value: &str) {
    output.push(' ');
    output.push_str(name);
    output.push_str("=\"");
    output.push_str(&xml_escape(value));
    output.push('"');
}

/// Escape XML delimiters after making spoofing-prone Unicode visible.
fn xml_escape(value: &str) -> String {
    visible_escape_metadata(value)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
