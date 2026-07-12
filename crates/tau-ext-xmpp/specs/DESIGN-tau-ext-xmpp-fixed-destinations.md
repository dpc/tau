# DESIGN-tau-ext-xmpp-fixed-destinations: Fixed conversation destinations

Status: unconfirmed

`xmpp_send` has no destination JID argument and sends only to the registered agent's configured conversation. Tool handlers reject unknown arguments even though the published JSON schemas also have `additionalProperties: false`; this preserves the no-model-chosen-destination invariant if a caller bypasses schema validation.
