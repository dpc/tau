# DECISION-common-external-message-envelope: Compact XML for external messages

Authority: confirmed, 2026-07-18, dpc

Messages projected into model context from Slack, XMPP, Telegram, and similar
external-message publishers use one compact, flat `<tau_message>` XML envelope.
Message text is its directly XML-escaped body; bounded attributes carry
transport-neutral publisher, event, opaque message/sender references,
presentation, conversation alias, sender-authentication result, and content-trust
metadata when applicable. Native transport identifiers are not exposed.

Tau and the publishing bridge establish metadata rather than accepting body
claims. A shared XML form was chosen over transport-specific prose or redundant
nested wrappers because it is compact, readable, and keeps authenticated
provenance distinct from untrusted content. Exact fields and projection behavior
are specified by
[SPEC-external-message-reports-and-facts](SPEC-external-message-reports-and-facts.md).
