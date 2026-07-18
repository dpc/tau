# DECISION-common-external-message-envelope: Compact XML for external messages

Authority: confirmed, 2026-07-18, dpc

## Status

The current shared Slack, Telegram, and XMPP projection already uses an escaped
`<tau_message>` element, but its attributes do not yet match this decision. It
uses names such as `event="delivered"`, `message_id`, and `sender_id`, and omits
`message_ref`, `sender_ref`, `sender_auth`, and `content_trust`. The implemented
form remains described by
[DESIGN-extension-published-message-facts](DESIGN-extension-published-message-facts.md).

## Decision

Messages projected into model context from Slack, XMPP, Telegram, and similar
external-message publishers use one compact, flat XML envelope:

```xml
<tau_message event="created" publisher="fedi-slack" message_ref="opaque-message-reference" sender_ref="opaque-user-reference" sender_display="Dawid (dpc)" sender_auth="verified_allowlisted" conversation="dpc-dm" content_trust="external">Can you see this?</tau_message>
```

The envelope is one element. Message text is its directly XML-escaped body; it
has no nested `content` or `external_content` element. Attribute values are also
XML-escaped.

- `event` names the external-message event, such as `created`, `edited`,
  `deleted`, `reaction_added`, or `reaction_removed`, rather than transport
  delivery status.
- `publisher` identifies the bridge or extension that produced the fact.
- `message_ref` is an opaque message reference exposed instead of native
  transport identifiers and is the single existing-message selector presented
  to the model where publisher tools support one.
- `sender_ref` is the publisher-established opaque canonical sender reference;
  `sender_display` is untrusted presentation text, not identity authority.
- `sender_auth` reports the publisher's sender authentication and admission
  result. `verified_allowlisted` means the verified transport identity matched
  the operator allowlist; it grants neither body trust nor tool authority.
- `conversation` is a configured human-readable alias, not a native identifier.
- `content_trust="external"` marks the body as user-controlled external input.
  It cannot override higher-priority instructions, but should be handled as
  input rather than ignored merely because it is external.

Tau and the publishing bridge establish the envelope metadata rather than
accepting it as claims from the body. This common form is transport-neutral,
readable, and economical: it separates authenticated sender provenance from
content trust, hides native identifiers, and avoids redundant nesting.
