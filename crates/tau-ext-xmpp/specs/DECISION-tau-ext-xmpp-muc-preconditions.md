# DECISION-tau-ext-xmpp-muc-preconditions: MUC deployment preconditions

Authority: unconfirmed

The MVP joins/creates MUC rooms, requests no history, waits for self-presence, and
submits instant-room config to unlock newly-created rooms. It does not fetch full
configuration forms or submit member affiliation IQs. Private, hidden, members-only,
persistent, and real-JID-visible room policy must be provided by the Prosody/server
defaults or preconfiguration.

Tau-side authorization still fails closed unless an inbound MUC occupant has a current
real-JID presence mapping that matches `allowed_jids`. Operators may set
`trust_muc_membership: true` only when they intentionally accept server-side MUC
membership as the security boundary for hidden-real-JID rooms.
