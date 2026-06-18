# Design decisions

This file records major design decisions currently embodied by this directory's code, and how authoritative each decision is. It is not an architecture overview, ADR log, todo list, roadmap, implementation guide, or changelog.

## Disabled opt-in personal bridge

Status: unconfirmed

`std-xmpp` is disabled by default and publishes only opt-in tools. Agents must call `xmpp_register(enabled: true)` before `xmpp_send` works.

## Fixed conversation destinations

Status: unconfirmed

`xmpp_send` has no destination JID argument and sends only to the registered agent's configured conversation. Tool handlers reject unknown arguments even though the published JSON schemas also have `additionalProperties: false`; this preserves the no-model-chosen-destination invariant if a caller bypasses schema validation.

## Mandatory allowlist and default recipient

Status: unconfirmed

`allowed_jids` is mandatory and `default_recipient` must match it. Bare allowlist entries match any sender resource for that account; full entries match exactly.

## Generated XMPP resources

Status: unconfirmed

The extension generates a high-entropy resource for the configured bare account JID and uses the server-returned bound JID. This lets multiple Tau processes share one XMPP account without resource conflicts.

## MUC conversation identity

Status: unconfirmed

Recommended routing is one MUC room per extension-worker session and agent. The room name includes a random worker token so concurrent Tau sessions do not collide.

## Direct-resource fallback scope

Status: unconfirmed

Direct-resource routing is a fallback. It supports one registered direct agent per extension instance because one bound full JID cannot distinguish multiple agents for inbound direct messages. On reconnect, a changed bound resource is announced to the default recipient.

## MUC deployment preconditions

Status: unconfirmed

The MVP joins/creates MUC rooms and requests no history. It does not submit XEP-0045 room configuration forms or member affiliation IQs. Private, hidden, members-only, persistent, and real-JID-visible room policy must be provided by the Prosody/server defaults or preconfiguration.

Tau-side authorization still fails closed unless an inbound MUC occupant has a current real-JID presence mapping that matches `allowed_jids`. Operators may set `trust_muc_membership: true` only when they intentionally accept server-side MUC membership as the security boundary for hidden-real-JID rooms.

## Plaintext-over-TLS security model

Status: unconfirmed

The MVP sends ordinary XMPP text protected by TLS certificate validation. It does not implement OMEMO or any other E2EE, so XMPP servers and room occupants can read message content. Incoming text is prefixed with XMPP source context and is submitted only via `extension.prompt_submit_request`.

## Unit-first testing strategy

Status: unconfirmed

Unit tests with fake or state-only XMPP surfaces cover config validation, opt-in tool metadata, send-before-register rejection, registration state, MUC real-JID allowlist routing, hidden-real-JID fail-closed behavior, explicit membership-trust behavior, own-message suppression, stale occupant cache invalidation, delayed/history drops, message-size drops, unknown tool-argument rejection, direct full-JID exact-to routing, and reconnect state updates. Live Prosody testing is still future work.

## Registration timeout rollback

Status: unconfirmed

Registration commands have an overall worker-side timeout shorter than the outer tool-call wait. If the caller has already timed out and dropped the response receiver anyway, the worker rolls back conversation maps so a failed registration cannot leave ghost XMPP routing state.
