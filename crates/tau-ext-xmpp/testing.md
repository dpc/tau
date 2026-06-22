# tau-ext-xmpp testing

Use `cargo test -p tau-ext-xmpp` for the crate's fast regression suite. The
unit tests cover configuration validation, tool schemas, registration/send
gating, bounded readiness waits, room-name derivation, MUC join confirmation,
history suppression, real-JID allowlist enforcement, direct-resource routing,
and shutdown handling.

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
5. Confirm Tau receives the reply as an external prompt annotated with XMPP
   room/source context.
6. Call `xmpp_send` and confirm the response appears in the same room.
7. Restart the XMPP connection and confirm registered MUC rooms rejoin, while
   delayed room history is not converted into fresh prompts.
8. Unregister the agent or shut down the session and confirm Tau sends
   unavailable presence / leaves the room on the server.

If the MUC service hides real JIDs, keep `trust_muc_membership: false` for the
default smoke test and verify replies are rejected. Only repeat with
`trust_muc_membership: true` when the server-side room membership list is the
intended authorization boundary.
