# tau-ext-xmpp testing

Use `cargo test -p tau-ext-xmpp` for the crate's fast regression suite. The
unit tests cover configuration validation, tool schemas, registration/send
gating, bounded readiness waits, default and custom room-template derivation,
strict helper/runtime validation, replayed role metadata, MUC join
confirmation, history suppression, real-JID allowlist enforcement, model-visible
fact metadata privacy, direct-resource routing, sent-fact ordering, and shutdown
handling.

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
5. Confirm Tau receives the reply as an external message fact whose sender and
   conversation metadata identify the accepted source without exposing
   actionable routes or the generated Tau occupant label.
6. Call `xmpp_send` and confirm the response appears in the same room.
7. Restart the XMPP connection and confirm registered MUC rooms rejoin, while
   delayed room history is not published as fresh message facts.
8. Unregister the agent or shut down the session and confirm Tau sends
   unavailable presence / leaves the room on the server.
9. Configure a role/group-based `muc.room_template`, resume Tau, and confirm the
   replayed agent role and current group render the expected room.
10. Configure `{{agent_id}}-{{random_alphanumeric 6}}`, unregister/re-register,
    and confirm the explicitly unstable policy creates a different room.

If the MUC service hides real JIDs, keep `trust_muc_membership: false` for the
default smoke test and verify replies are rejected. Only repeat with
`trust_muc_membership: true` when the server-side room membership list is the
intended authorization boundary.
