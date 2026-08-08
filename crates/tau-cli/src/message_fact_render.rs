//! Bounded terminal rendering for committed canonical external-message facts.

#[cfg(test)]
#[path = "message_fact_render/tests.rs"]
mod tests;

use tau_proto::{
    AgentId, Event, MessageAgentTarget, MessageConversation, MessageFactRef, MessageParty,
    MessagePublisherId, project_message_fact, visible_escape_metadata,
};

use crate::terminal_text::sanitize_terminal_body;

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

    /// Return the concise human-readable event description.
    fn description(&self) -> &'static str {
        match self {
            Self::Delivered(_) => "message",
            Self::Edited(_) => "message edited",
            Self::Deleted(_) => "message deleted",
            Self::ReactionAdded(_) => "reaction added",
            Self::ReactionRemoved(_) => "reaction removed",
            Self::Sent(_) => "message sent",
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

    /// Borrow the event-specific participant and its directional preposition.
    fn party(&self) -> Option<(&'static str, &MessageParty)> {
        match self {
            Self::Delivered(fact) => Some(("from", &fact.sender)),
            Self::Edited(fact) => fact.actor.as_ref().map(|party| ("by", party)),
            Self::Deleted(fact) => fact.actor.as_ref().map(|party| ("by", party)),
            Self::ReactionAdded(fact) => fact.actor.as_ref().map(|party| ("by", party)),
            Self::ReactionRemoved(fact) => fact.actor.as_ref().map(|party| ("by", party)),
            Self::Sent(fact) => fact.recipient.as_ref().map(|party| ("to", party)),
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
        "External `{}` {}",
        escape(view.publisher().as_str()),
        view.description()
    );
    let has_party = view.party().is_some();
    let has_conversation = view.conversation().is_some();
    if let Some((preposition, party)) = view.party() {
        output.push(' ');
        output.push_str(preposition);
        output.push_str(" \"");
        output.push_str(&escape_quoted(preferred_party_name(party)));
        output.push('"');
    }
    if let Some(conversation) = view.conversation() {
        output.push_str(" in ");
        push_context_value(&mut output, preferred_conversation_name(conversation));
    }
    if !has_party
        && !has_conversation
        && let Some(reference) = view.reference()
    {
        output.push_str(" for message `");
        output.push_str(&escape(reference.publisher_extension_id.as_str()));
        output.push_str("`/");
        push_context_value(&mut output, reference.message_id.as_str());
    }
    if matches!(target_context, MessageFactTargetContext::Explicit) {
        output.push_str(" for Tau target ");
        push_context_value(&mut output, view.agent_id().as_str());
    }
    output.push(':');
    if let Some(text) = view.text() {
        output.push('\n');
        output.push_str(&sanitize_terminal_body(text));
    } else if let Some(reaction) = view.reaction() {
        output.push('\n');
        output.push_str(&escape(reaction));
    }
    Some(output)
}

/// Choose a participant's useful display label, or its stable ID as fallback.
fn preferred_party_name(party: &MessageParty) -> &str {
    party
        .display_name
        .as_deref()
        .filter(|display| useful_display(display))
        .unwrap_or(&party.stable_id)
}

/// Choose a conversation display, alias, or stable ID in decreasing preference.
fn preferred_conversation_name(conversation: &MessageConversation) -> &str {
    conversation
        .display_name
        .as_deref()
        .filter(|display| useful_display(display))
        .or_else(|| {
            conversation
                .alias
                .as_deref()
                .filter(|alias| useful_display(alias))
        })
        .unwrap_or(&conversation.stable_id)
}

/// Return whether optional presentation metadata communicates a visible label.
fn useful_display(display: &str) -> bool {
    display.chars().any(|character| !character.is_whitespace())
}

/// Append context unquoted when unambiguous, otherwise as an escaped quoted
/// value.
fn push_context_value(output: &mut String, value: &str) {
    if value.chars().all(context_character_is_unambiguous) {
        output.push_str(&escape(value));
    } else {
        output.push('"');
        output.push_str(&escape_quoted(value));
        output.push('"');
    }
}

/// Return whether a character cannot imitate surrounding heading prose.
fn context_character_is_unambiguous(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '-' | '_' | '.' | '/' | '#' | '@')
}

/// Escape a value enclosed in double quotes.
fn escape_quoted(value: &str) -> String {
    escape(value).replace('"', r#"\""#)
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
