# SPEC-tau-ext-xmpp-muc-lifecycle: XMPP MUC lifecycle

Registration sends join presence and retains the exact room/nick as pending and
non-routable. It waits for matching self-presence or a presence error. Status 201 for a
newly created room requires successful XEP-0045 instant-room owner config. Only after
join confirmation and any required unlock does the worker promote the route and deliver
registration success.

After the success response, the worker sends a mediated invite to the allowlisted
`default_recipient`, followed by a direct fallback notice containing the room JID. Both
are best effort, shutdown-cancellable, and cannot fail or extend the registration
success path. Rollback, unregister, and shutdown send unavailable presence for tracked
pending or active rooms while connected.

The MVP requests no history and drops delayed/history messages. It does not fetch full
room forms or set member affiliations; private, hidden, members-only, persistent, and
real-JID-visible policy comes from server defaults or preconfiguration. Inbound occupant
authorization requires a current occupant-to-real-JID mapping matching `allowed_jids`,
unless the operator explicitly sets `trust_muc_membership: true` and accepts server
membership as the security boundary. Disconnect clears those mappings.

Registration has a 45-second worker budget inside the outer 60-second tool wait.
Timeout, join/config failure, or a dropped response receiver removes pending and active
routing and sends unavailable presence when connected. Worker-wide shutdown promptly
interrupts or bounds readiness, join/rejoin, reconnect, send, and notice work so leave
cleanup gets the remaining shutdown budget.

Room identity and accepted risks are recorded by
[DECISION-tau-ext-xmpp-muc-identity](DECISION-tau-ext-xmpp-muc-identity.md),
[DECISION-tau-ext-xmpp-muc-preconditions](DECISION-tau-ext-xmpp-muc-preconditions.md),
and [DECISION-tau-ext-xmpp-muc-lifecycle](DECISION-tau-ext-xmpp-muc-lifecycle.md).
