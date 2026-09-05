#[cfg(test)]
mod tests;
use std::borrow::Cow;

/// The model-context carrier in which a registered payload envelope appears.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadEnvelopeCarrier {
    /// A complete free-form text part in a user-role message.
    GenericUserText,
    /// Canonical message-fact text projected in user or assistant role
    /// according to the typed event direction.
    GenericUserOrAssistantText,
    /// A protocol-typed tool result whose body carries its own external-data
    /// frame.
    TypedToolResult,
}

/// Opening-sentinel shape for one registered payload-envelope family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadEnvelopeOpening {
    /// The complete opening sentinel has no attributes.
    Fixed(&'static str),
    /// The opening sentinel starts with the tag name followed by registered
    /// attributes.
    Attributed(&'static str),
}

/// Shared metadata for one top-level XML-lite payload-envelope family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisteredPayloadEnvelope {
    /// Stable XML-lite tag name.
    pub name: &'static str,
    /// Fixed or attributed opening-sentinel shape.
    pub opening: PayloadEnvelopeOpening,
    /// Dynamic attribute names in deterministic rendering order.
    pub ordered_attributes: &'static [&'static str],
    /// Exact trusted closing sentinel appended by the owning renderer.
    pub exact_close: &'static str,
    /// Visible replacement for exact closing-sentinel collisions in payload
    /// text.
    pub visible_close: &'static str,
    /// Model-context carrier in which the outer family is used.
    pub carrier: PayloadEnvelopeCarrier,
}

impl RegisteredPayloadEnvelope {
    /// Neutralize this family's exact close while preserving every other byte.
    #[must_use]
    pub fn escape_body<'a>(self, body: &'a str) -> Cow<'a, str> {
        escape_exact_sentinel_close(body, self.exact_close, self.visible_close)
    }

    /// Return whether text is exactly one lexically closed instance of this
    /// family.
    #[must_use]
    pub fn matches_whole(self, text: &str) -> bool {
        let body = match self.opening {
            PayloadEnvelopeOpening::Fixed(open) => text.strip_prefix(open),
            PayloadEnvelopeOpening::Attributed(prefix) => text
                .strip_prefix(prefix)
                .and_then(|rest| rest.find('>').map(|end| &rest[end + 1..])),
        };
        body.and_then(|body| body.strip_suffix(self.exact_close))
            .is_some_and(|body| !body.contains(self.exact_close))
    }
}

/// Registered fieldless envelope for authenticated interactive user prompts.
pub const USER_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = RegisteredPayloadEnvelope {
    name: "user",
    opening: PayloadEnvelopeOpening::Fixed("<user>"),
    ordered_attributes: &[],
    exact_close: "</user>",
    visible_close: "&lt;/user&gt;",
    carrier: PayloadEnvelopeCarrier::GenericUserText,
};

/// Registered fieldless envelope for harness-authenticated asynchronous input.
pub const TAU_INTERNAL_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = RegisteredPayloadEnvelope {
    name: "tau_internal",
    opening: PayloadEnvelopeOpening::Fixed("<tau_internal>"),
    ordered_attributes: &[],
    exact_close: "</tau_internal>",
    visible_close: "&lt;/tau_internal&gt;",
    carrier: PayloadEnvelopeCarrier::GenericUserText,
};

/// Registered attributed envelope for canonical external message facts.
pub const MESSAGE_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = RegisteredPayloadEnvelope {
    name: "message",
    opening: PayloadEnvelopeOpening::Attributed("<message "),
    ordered_attributes: &[
        "event",
        "publisher",
        "message_ref",
        "sender_ref",
        "sender_display",
        "sender_auth",
        "recipient_ref",
        "recipient_display",
        "conversation",
        "reaction",
        "content_trust",
    ],
    exact_close: "</message>",
    visible_close: "&lt;/message&gt;",
    carrier: PayloadEnvelopeCarrier::GenericUserOrAssistantText,
};

/// Registered attributed envelope for typed web-search tool results.
pub const TAU_WEB_CONTENT_PAYLOAD_ENVELOPE: RegisteredPayloadEnvelope = RegisteredPayloadEnvelope {
    name: "tau_web_content",
    opening: PayloadEnvelopeOpening::Attributed("<tau_web_content "),
    ordered_attributes: &["adapter", "operation", "content_trust"],
    exact_close: "</tau_web_content>",
    visible_close: "&lt;/tau_web_content&gt;",
    carrier: PayloadEnvelopeCarrier::TypedToolResult,
};

/// Return every registered top-level model-facing payload-envelope family.
#[must_use]
pub const fn registered_payload_envelopes() -> &'static [RegisteredPayloadEnvelope] {
    &[
        USER_PAYLOAD_ENVELOPE,
        TAU_INTERNAL_PAYLOAD_ENVELOPE,
        MESSAGE_PAYLOAD_ENVELOPE,
        TAU_WEB_CONTENT_PAYLOAD_ENVELOPE,
    ]
}

/// Return registered families used by the shared generic user-role text
/// carrier.
pub fn generic_user_payload_envelopes() -> impl Iterator<Item = &'static RegisteredPayloadEnvelope>
{
    registered_payload_envelopes().iter().filter(|family| {
        matches!(
            family.carrier,
            PayloadEnvelopeCarrier::GenericUserText
                | PayloadEnvelopeCarrier::GenericUserOrAssistantText
        )
    })
}

/// Replace only one envelope family's exact closing sentinel in an assembled
/// body.
///
/// Callers supply hard-coded `exact_close` and `visible_close` constants, apply
/// domain normalization and bounds before this function, and append the trusted
/// exact close afterward. The returned body preserves every nonmatching byte.
#[must_use]
pub fn escape_exact_sentinel_close<'a>(
    body: &'a str,
    exact_close: &str,
    visible_close: &str,
) -> Cow<'a, str> {
    if body.contains(exact_close) {
        Cow::Owned(body.replace(exact_close, visible_close))
    } else {
        Cow::Borrowed(body)
    }
}
