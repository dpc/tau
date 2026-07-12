# DESIGN-tau-ext-xmpp-readiness-waits: Bounded readiness waits

Status: unconfirmed

`xmpp_register(enabled: true)` starts the bridge and waits up to 30 seconds for the worker to observe an authenticated XMPP `Online` event before creating the conversation. `xmpp_send` waits up to 30 seconds only when the bridge has already been started; it does not turn send-before-register into an implicit registration/start operation, and a missing conversation after readiness still returns the explicit `xmpp_register(enabled: true)` requirement. Readiness is owned by the worker, which processes intervening stanzas while waiting and clears connection-scoped online/occupant caches on disconnect so later commands require a fresh `Online` event.
