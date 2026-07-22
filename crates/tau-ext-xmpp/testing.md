# XMPP testing

Tests are unit-first and use fake or state-only XMPP surfaces. They cover config
validation, opt-in metadata, send-before-register rejection, registration state,
bounded readiness and timeout propagation, disconnect cache invalidation,
multiple MUC agents, room identity/collision handling, status-201 room config,
long-agent-ID and nodeprep/case-fold non-collapse, distinct active and pending
room collisions, join errors and exact self-presence correlation, mediated
invite payloads and leave presence, real-JID allowlist routing,
hidden-real-JID fail-closed and explicit membership trust behavior, exact
transport-neutral delivered-report mapping, report-before-result ordering and
`persist=false` metadata,
own/delayed/history/oversize drops, strict tool arguments, direct exact-to
full-JID routing, and reconnect state updates.

Live Prosody testing is a manual smoke test. Follow the existing checklist below;
never put live credentials into automated fixtures.

## Live Prosody smoke test

For a live Prosody smoke test, use a private test account and MUC component:

1. Configure the extension as shown in `README.md`, with `routing.mode: muc`,
   a non-empty `allowed_jids`, and the test human account as
   `default_recipient`.
2. Start Tau with a role that explicitly enables `xmpp_register` and
   `xmpp_send`.
3. Call `xmpp_register(enabled: true)` from one agent and confirm the tool
   returns a room JID.
4. Confirm the human client receives the mediated invite or the direct fallback
   notice, joins the room, and replies in the room.
5. Confirm Tau receives the reply as a harness-authored canonical external-message fact whose sender and
   conversation metadata identify the accepted source without exposing
   actionable routes or the generated Tau occupant label.
6. Call `xmpp_send` and confirm the response appears in the same room.
7. Restart the XMPP connection and confirm registered MUC rooms rejoin, while
   delayed room history is not submitted as fresh message reports.
8. Unregister the agent or shut down the session and confirm Tau sends
   unavailable presence / leaves the room on the server.
9. Configure a role/group-based `muc.room_template`, resume Tau, and confirm the
   replayed agent role and current group render the expected room.
10. Configure `{{agent_id}}-{{random_alphanumeric 6}}`, unregister/re-register,
    and confirm the explicitly unstable policy creates a different room.

Automated extension tests own bridge-local admission and report/result ordering.
Harness tests own interception, canonical durability, replay, projection, and
wake.

If the MUC service hides real JIDs, keep `trust_muc_membership: false` for the
default smoke test and verify replies are rejected. Only repeat with
`trust_muc_membership: true` when the server-side room membership list is the
intended authorization boundary.
