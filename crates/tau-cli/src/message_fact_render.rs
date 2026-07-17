//! Bounded terminal rendering for committed extension-published message facts.

#[cfg(test)]
#[path = "message_fact_render/tests.rs"]
mod tests;

use tau_proto::{
    AgentId, Event, MessageAgentTarget, MessageConversation, MessageFactId, MessageFactRef,
    MessageParty, MessagePublisherId, project_message_fact, visible_escape_metadata,
};

/// Exhaustive borrowed UI view of one concrete message fact.
enum UiMessageFact<'a> {
    /// Delivered base message.
    Delivered(&'a tau_proto::MessageDelivered),
    /// Edited referenced message.
    Edited(&'a tau_proto::MessageEdited),
    /// Deleted referenced message.
    Deleted(&'a tau_proto::MessageDeleted),
    /// Reaction added to a referenced message.
    ReactionAdded(&'a tau_proto::MessageReactionAdded),
    /// Reaction removed from a referenced message.
    ReactionRemoved(&'a tau_proto::MessageReactionRemoved),
    /// Successfully sent base message.
    Sent(&'a tau_proto::MessageSent),
}

impl<'a> UiMessageFact<'a> {
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

    /// Return the stable event name used by deterministic diagnostics.
    fn event_name(&self) -> &'static str {
        match self {
            Self::Delivered(_) => "message.delivered",
            Self::Edited(_) => "message.edited",
            Self::Deleted(_) => "message.deleted",
            Self::ReactionAdded(_) => "message.reaction_added",
            Self::ReactionRemoved(_) => "message.reaction_removed",
            Self::Sent(_) => "message.sent",
        }
    }

    /// Return the distinct human-readable operation heading.
    fn heading(&self) -> &'static str {
        match self {
            Self::Delivered(_) => "External message delivered",
            Self::Edited(_) => "External message edited",
            Self::Deleted(_) => "External message deleted",
            Self::ReactionAdded(_) => "External message reaction added",
            Self::ReactionRemoved(_) => "External message reaction removed",
            Self::Sent(_) => "External message sent",
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

    /// Borrow the raw claimed transcript target.
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

    /// Borrow a publisher-scoped base-message identifier.
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

    /// Borrow an unresolved publisher-scoped target reference.
    fn reference(&self) -> Option<&MessageFactRef> {
        match self {
            Self::Edited(fact) => Some(&fact.target),
            Self::Deleted(fact) => Some(&fact.target),
            Self::ReactionAdded(fact) => Some(&fact.target),
            Self::ReactionRemoved(fact) => Some(&fact.target),
            Self::Delivered(_) | Self::Sent(_) => None,
        }
    }

    /// Borrow the event-specific participant and its UI label.
    fn party(&self) -> Option<(&'static str, &MessageParty)> {
        match self {
            Self::Delivered(fact) => Some(("Sender", &fact.sender)),
            Self::Edited(fact) => fact.actor.as_ref().map(|party| ("Actor", party)),
            Self::Deleted(fact) => fact.actor.as_ref().map(|party| ("Actor", party)),
            Self::ReactionAdded(fact) => fact.actor.as_ref().map(|party| ("Actor", party)),
            Self::ReactionRemoved(fact) => fact.actor.as_ref().map(|party| ("Actor", party)),
            Self::Sent(fact) => fact.recipient.as_ref().map(|party| ("Recipient", party)),
        }
    }

    /// Borrow optional conversation provenance.
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

    /// Borrow the publisher-defined reaction for reaction facts.
    fn reaction(&self) -> Option<&str> {
        match self {
            Self::ReactionAdded(fact) => Some(&fact.reaction),
            Self::ReactionRemoved(fact) => Some(&fact.reaction),
            Self::Delivered(_) | Self::Edited(_) | Self::Deleted(_) | Self::Sent(_) => None,
        }
    }

    /// Borrow the untrusted body for text-bearing facts.
    fn text(&self) -> Option<&str> {
        match self {
            Self::Delivered(fact) => Some(&fact.text),
            Self::Edited(fact) => Some(&fact.text),
            Self::Sent(fact) => Some(&fact.text),
            Self::Deleted(_) | Self::ReactionAdded(_) | Self::ReactionRemoved(_) => None,
        }
    }
}

/// Parsed routing disposition for a message fact's claimed Tau target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MessageFactTarget {
    /// Syntactically valid target that may correspond to a loaded transcript.
    Valid(AgentId),
    /// Invalid raw target retained only for deterministic global diagnostics.
    Invalid,
}

/// Whether the containing UI transcript already identifies the fact's target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MessageFactTargetContext {
    /// A global view must render the fact's claimed Tau target explicitly.
    Explicit,
    /// The containing target transcript already identifies the Tau target.
    Implied,
}

/// Return the parsed claimed target disposition for a message fact.
///
/// Invalid targets stay visible as global diagnostics instead of being assigned
/// to a transcript.
pub(super) fn target_agent_id(event: &Event) -> Option<MessageFactTarget> {
    let view = UiMessageFact::from_event(event)?;
    Some(
        AgentId::parse(view.agent_id().as_str())
            .map(MessageFactTarget::Valid)
            .unwrap_or(MessageFactTarget::Invalid),
    )
}

/// Render one valid or deterministically unprojectable message fact.
///
/// `target_context` identifies whether the containing transcript already names
/// the claimed Tau target. Opaque extension data is intentionally never read.
pub(super) fn render(event: &Event, target_context: MessageFactTargetContext) -> Option<String> {
    let view = UiMessageFact::from_event(event)?;
    let classification = project_message_fact(event)
        .expect("the shared classifier recognizes every UI message fact variant");
    if let Err(reason) = classification {
        return Some(format!(
            "Unprojectable message fact\nEvent: {}\nPublisher: {}\nReason: {}",
            view.event_name(),
            escape(view.publisher().as_str()),
            reason.as_str()
        ));
    }

    let mut output = format!(
        "{}\nPublisher: {}",
        view.heading(),
        escape(view.publisher().as_str())
    );
    if matches!(target_context, MessageFactTargetContext::Explicit) {
        push_field(&mut output, "Tau target", view.agent_id().as_str());
    }
    if let Some(message_id) = view.message_id() {
        push_field(&mut output, "Message ID", message_id.as_str());
    }
    if let Some(reference) = view.reference() {
        push_field(
            &mut output,
            "Referenced publisher",
            reference.publisher_extension_id.as_str(),
        );
        push_field(
            &mut output,
            "Referenced message ID",
            reference.message_id.as_str(),
        );
    }
    if let Some((label, party)) = view.party() {
        push_party(&mut output, label, party);
    }
    if let Some(conversation) = view.conversation() {
        push_described_id(
            &mut output,
            "Conversation",
            &conversation.stable_id,
            conversation.display_name.as_deref(),
        );
    }
    if let Some(reaction) = view.reaction() {
        push_field(&mut output, "Reaction", reaction);
    }
    if let Some(text) = view.text() {
        output.push_str("\nText:\n");
        output.push_str(&escape(text));
    }
    Some(output)
}

/// Append one escaped label/value line.
fn push_field(output: &mut String, label: &str, value: &str) {
    output.push('\n');
    output.push_str(label);
    output.push_str(": ");
    output.push_str(&escape(value));
}

/// Append a participant using its stable ID as the primary identifier.
fn push_party(output: &mut String, label: &str, party: &MessageParty) {
    push_described_id(
        output,
        label,
        &party.stable_id,
        party.display_name.as_deref(),
    );
}

/// Append a stable identifier and an optional secondary display label.
fn push_described_id(output: &mut String, label: &str, stable_id: &str, display: Option<&str>) {
    push_field(output, label, stable_id);
    if let Some(display) = display {
        output.push_str(" [display: ");
        output.push_str(&escape(display));
        output.push(']');
    }
}

/// Make control characters and presentation delimiters visibly injective.
///
/// Backslash is escaped before it can collide with a generated Unicode escape,
/// and brackets are escaped before they can imitate the secondary display-label
/// syntax.
fn escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut plain_start = 0;
    for (index, character) in value.char_indices() {
        let replacement = match character {
            '\\' => Some(r"\\"),
            '[' => Some(r"\["),
            ']' => Some(r"\]"),
            _ => None,
        };
        let Some(replacement) = replacement else {
            continue;
        };
        output.push_str(&visible_escape_metadata(&value[plain_start..index]));
        output.push_str(replacement);
        plain_start = index + character.len_utf8();
    }
    output.push_str(&visible_escape_metadata(&value[plain_start..]));
    output
}
