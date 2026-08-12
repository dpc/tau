# SPEC-tau-ext-xmpp-readiness-waits: Bounded readiness waits

## Status

This record describes the current implementation. The checked mandatory
publication boundary from
[SPEC-tau-ext-xmpp-tool-delivery-lifecycle](SPEC-tau-ext-xmpp-tool-delivery-lifecycle.md)
is current under the approved audit fix. Its prospective executor will move
readiness off the serialized reader and clamp this 30-second cap to one absolute
60-second reservation-to-terminal deadline. That remaining implementation is
not authorized by the prospective record.

## Record justification

Readiness behavior spans tool handlers, bridge command/response waits, and the
worker's online and reconnect state. These areas jointly determine whether a
command may proceed, time out, or require a fresh authenticated `Online` event.

`xmpp_register(enabled: true)` starts the bridge and waits up to 30 seconds for the
worker to observe an authenticated XMPP `Online` event before creating the conversation.
`xmpp_send` waits up to 30 seconds only when the bridge has already been started; it
does not turn send-before-register into an implicit registration/start operation, and a
missing conversation after readiness still returns the explicit `xmpp_register(enabled:
true)` requirement. Readiness is owned by the worker, which processes intervening
stanzas while waiting and clears connection-scoped online/occupant caches on disconnect
so later commands require a fresh `Online` event.

The worker registration budget is 45 seconds and remains shorter than the outer
60-second command wait, leaving time for rollback and bounded shutdown cleanup.
Disconnect clears the bound JID and occupant-to-real-JID mappings; later commands
require a fresh authenticated `Online` event.
