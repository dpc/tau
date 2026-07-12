# DESIGN-tau-ext-xmpp-testing-strategy: Unit-first testing strategy

Status: unconfirmed

See `testing.md` for the current fast unit-test expectations and a live Prosody
smoke-test checklist.

Unit tests with fake or state-only XMPP surfaces cover config validation, opt-in tool metadata, send-before-register rejection, registration state, bounded register/send readiness waits and timeout propagation, disconnect readiness/cache invalidation, multiple MUC agents in one Tau session, stable agent-only room identity, long-agent-id and case-folding non-collapse, generated-room active/pending collision rejection, MUC self-presence status 201 detection, MUC join error surfacing, exact room/nick join correlation, MUC mediated invite payloads, MUC leave presence construction, MUC real-JID allowlist routing, hidden-real-JID fail-closed behavior, explicit membership-trust behavior, model-visible MUC/direct prompt prefixes, prompt-label sanitization, own-message suppression, stale occupant cache invalidation, delayed/history drops, message-size drops, unknown tool-argument rejection, direct full-JID exact-to routing, and reconnect state updates. Live Prosody testing is documented as a manual smoke test.
