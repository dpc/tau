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

Recommended routing is one MUC room per Tau session id and agent id. The room name uses `<room_prefix>-<session-slug>-<agent-slug>-<8-char-disambiguator>`, where the slugs are short normalized lowercase hints and the disambiguator is compact base32 over a domain-separated BLAKE3 label of the full session id and full validated agent id. This keeps resumed Tau sessions on the same XMPP room while different sessions and agents remain collision-resistant separate conversations, without exposing long raw Tau ids or creating unbounded room localparts. The readable slug identity must not be treated as authoritative: the short disambiguator covers valid `AgentId`s and `SessionId`s that differ only by case, by generated suffix, or after slug truncation, but it is intentionally not injective. If a normalized generated room is already active or pending for a different agent, the worker rejects registration before join/routing insertion instead of overwriting `room_to_agent`.

## MUC invitations and lifecycle

Status: unconfirmed

MUC registration sends a join presence, stores the room/nick as a pending non-routable join, waits for the exact `room/nick` self-presence or presence error, and submits an XEP-0045 instant-room owner config if the self-presence reports status 201 for a newly-created room. Only after join confirmation and any required room unlock succeeds does registration promote the join to a routable conversation and send the `xmpp_register` success response. The formal XEP-0045 mediated invite to `default_recipient`, followed by a direct fallback notice containing the room JID for clients that do not surface mediated invites, happens after that success response. Invite and fallback notice delivery are best-effort after the room is usable; they are cancelled on shutdown and must not fail or extend the registration success path. Rollback, unregister, and session shutdown send unavailable presence for tracked pending or active rooms where the worker is still connected.

## Direct-resource fallback scope

Status: unconfirmed

Direct-resource routing is a fallback. It supports one registered direct agent per extension instance because one bound full JID cannot distinguish multiple agents for inbound direct messages. The second-registration error explicitly points users to `routing.mode: muc` for multiple agents or separate conversations. On reconnect, a changed bound resource is announced to the default recipient.

## Bounded readiness waits

Status: unconfirmed

`xmpp_register(enabled: true)` starts the bridge and waits up to 30 seconds for the worker to observe an authenticated XMPP `Online` event before creating the conversation. `xmpp_send` waits up to 30 seconds only when the bridge has already been started; it does not turn send-before-register into an implicit registration/start operation, and a missing conversation after readiness still returns the explicit `xmpp_register(enabled: true)` requirement. Readiness is owned by the worker, which processes intervening stanzas while waiting and clears connection-scoped online/occupant caches on disconnect so later commands require a fresh `Online` event.

## MUC deployment preconditions

Status: unconfirmed

The MVP joins/creates MUC rooms, requests no history, waits for self-presence, and submits instant-room config to unlock newly-created rooms. It does not fetch full configuration forms or submit member affiliation IQs. Private, hidden, members-only, persistent, and real-JID-visible room policy must be provided by the Prosody/server defaults or preconfiguration.

Tau-side authorization still fails closed unless an inbound MUC occupant has a current real-JID presence mapping that matches `allowed_jids`. Operators may set `trust_muc_membership: true` only when they intentionally accept server-side MUC membership as the security boundary for hidden-real-JID rooms.

## Plaintext-over-TLS security model

Status: unconfirmed

The MVP sends ordinary XMPP text protected by TLS certificate validation. It does not implement OMEMO or any other E2EE, so XMPP servers and room occupants can read message content. Incoming text is prefixed with XMPP source context and is submitted only via `extension.prompt_submit_request`.

## Unit-first testing strategy

Status: unconfirmed

See `testing.md` for the current fast unit-test expectations and a live Prosody
smoke-test checklist.

Unit tests with fake or state-only XMPP surfaces cover config validation, opt-in tool metadata, send-before-register rejection, registration state, bounded register/send readiness waits and timeout propagation, disconnect readiness/cache invalidation, multiple MUC agents in one Tau session, stable session/agent room identity, long-session-id/long-agent-id and case-folding non-collapse, generated-room active/pending collision rejection, MUC self-presence status 201 detection, MUC join error surfacing, exact room/nick join correlation, MUC mediated invite payloads, MUC leave presence construction, MUC real-JID allowlist routing, hidden-real-JID fail-closed behavior, explicit membership-trust behavior, own-message suppression, stale occupant cache invalidation, delayed/history drops, message-size drops, unknown tool-argument rejection, direct full-JID exact-to routing, and reconnect state updates. Live Prosody testing is documented as a manual smoke test.

## Registration timeout rollback

Status: unconfirmed

Registration commands have an overall worker-side timeout shorter than the outer tool-call wait. If registration times out after a successful MUC join, or if the caller has already timed out and dropped the response receiver anyway, the worker rolls back conversation maps and sends unavailable presence so a failed registration cannot leave ghost XMPP routing state or a stale room occupant. Shutdown is a worker-wide cancellation source: in-flight readiness, join, rejoin, reconnect, stanza-send, and best-effort notice work must be interrupted or bounded so unavailable presence cleanup gets the remaining shutdown budget. Best-effort invite/fallback notices are sent only after the success response and are cancelled by shutdown so unavailable presence cleanup is prioritized under the bounded shutdown budget.
