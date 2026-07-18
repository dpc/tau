# DECISION-tau-ext-xmpp-muc-lifecycle: MUC invitations and lifecycle

Authority: unconfirmed

MUC registration sends a join presence, stores the room/nick as a pending non-routable
join, waits for the exact `room/nick` self-presence or presence error, and submits an
XEP-0045 instant-room owner config if the self-presence reports status 201 for a
newly-created room. Only after join confirmation and any required room unlock succeeds
does registration promote the join to a routable conversation and send the
`xmpp_register` success response. The formal XEP-0045 mediated invite to
`default_recipient`, followed by a direct fallback notice containing the room JID for
clients that do not surface mediated invites, happens after that success response.
Invite and fallback notice delivery are best-effort after the room is usable; they are
cancelled on shutdown and must not fail or extend the registration success path.
Rollback, unregister, and session shutdown send unavailable presence for tracked pending
or active rooms where the worker is still connected.
