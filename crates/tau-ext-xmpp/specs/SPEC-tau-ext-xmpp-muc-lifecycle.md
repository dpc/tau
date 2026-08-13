# SPEC-tau-ext-xmpp-muc-lifecycle: XMPP MUC lifecycle

## Status

This record describes current MUC behavior. The checked mandatory publication
boundary from
[SPEC-tau-ext-xmpp-tool-delivery-lifecycle](SPEC-tau-ext-xmpp-tool-delivery-lifecycle.md)
is current under the approved audit fix. Exact process-local registration leases
now revoke local routing before best-effort cleanup, and stale cleanup cannot
remove a newer route. The prospective executor will place registration and
remote cleanup in one FIFO and clamp registration to the whole-intent deadline.
That remaining executor/deadline implementation is not authorized by the
prospective record.

## Record justification

MUC lifecycle spans configuration and room rendering, pending and active route
state, stanza handling and authorization, registration completion and rollback,
and shutdown cleanup. No one area defines safe route installation and removal.

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
membership as the security boundary.

The worker retains mappings only for available full occupant-JID presence from
an active room or the exact pending room currently joining. It accepts at most
256 mappings per room and 1,024 across the worker, including those exact limits;
replacement of an existing occupant remains allowed at capacity. Overflow clears
and quarantines only that room. A quarantined active room drops all groupchat
before either real-JID or trusted-membership admission, while initial-roster
overflow fails registration. A fresh join rebuilds that room from empty state.
Rollback, retirement, disconnect, a new online connection, and shutdown purge
the applicable mappings and quarantine state.

Registration has a 45-second worker budget inside the outer 60-second tool wait.
Timeout, join/config failure, or a dropped response receiver removes pending and active
routing and sends unavailable presence when connected. Worker-wide shutdown promptly
interrupts or bounds readiness, join/rejoin, reconnect, send, and notice work so leave
cleanup gets the remaining shutdown budget.
