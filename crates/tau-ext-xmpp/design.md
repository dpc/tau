# tau-ext-xmpp design notes

## Design decisions

- Status: accepted for the plaintext MVP. `std-xmpp` is disabled by default and publishes only opt-in tools.
- Status: accepted for the plaintext MVP. `xmpp_register(enabled: true)` is required before `xmpp_send`; `xmpp_send` has
  no destination JID argument.
- Status: accepted for the plaintext MVP. `allowed_jids` is mandatory and `default_recipient` must match it. Bare
  allowlist entries match any sender resource for that account; full entries
  match exactly.
- Status: accepted for the plaintext MVP. The extension generates a high-entropy resource for the configured bare account
  JID and uses the server-returned bound JID. This lets multiple Tau processes
  share one XMPP account without resource conflicts.
- Status: accepted for the plaintext MVP. Recommended routing is one MUC room per extension-worker session and agent. The
  room name includes a random worker token so concurrent Tau sessions do not
  collide.
- Status: accepted for the plaintext MVP. Direct-resource routing is a fallback. The current fallback supports one
  registered direct agent per extension instance because one bound full JID
  cannot distinguish multiple agents for inbound direct messages.

## MUC deployment preconditions

Status: accepted for the plaintext MVP; implementing XEP-0045 room configuration
and affiliation management is deferred.

The MVP joins/creates MUC rooms and requests no history. It does not submit
XEP-0045 room configuration forms or member affiliation IQs. Private, hidden,
members-only, persistent, and real-JID-visible room policy must be provided by
the Prosody/server defaults or preconfiguration.

Tau-side authorization still fails closed unless an inbound MUC occupant has a
current real-JID presence mapping that matches `allowed_jids`. Operators may set
`trust_muc_membership: true` only when they intentionally accept server-side MUC
membership as the security boundary for hidden-real-JID rooms.

## Security model

Status: accepted for the plaintext MVP; E2EE/OMEMO is deferred.

The MVP sends ordinary XMPP text protected by TLS certificate validation. It does
not implement OMEMO or any other E2EE, so XMPP servers and room occupants can
read message content. Incoming text is prefixed with XMPP source context and is
submitted only via `extension.prompt_submit_request`.

## Testing strategy

Status: accepted for the plaintext MVP; live Prosody integration testing is
deferred.

Unit tests cover config validation, opt-in tool metadata, send-before-register
rejection, registration state, MUC real-JID allowlist routing, hidden-real-JID
fail-closed behavior, explicit membership-trust behavior, own-message
suppression, stale occupant cache invalidation, message-size drops, and direct
full-JID exact-to routing. Live Prosody testing is still future work.

## Timeout and rollback behavior

Status: accepted for the plaintext MVP. Registration commands have an overall
worker-side timeout shorter than the outer tool-call wait. If the caller has
already timed out and dropped the response receiver anyway, the worker rolls
back conversation maps so a failed registration cannot leave ghost XMPP routing
state.
