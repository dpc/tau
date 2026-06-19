# tau-ext-xmpp architecture

`std-xmpp` is a disabled-by-default personal XMPP text bridge. It exposes only
`xmpp_register` and `xmpp_send`; the model never supplies arbitrary destination
JIDs. The extension does not connect to XMPP until an agent registers.

A single XMPP account may be used by multiple Tau processes concurrently because
this extension always requests a generated, high-entropy resource and then uses
the server-returned bound full JID.

The preferred routing mode is one MUC room per registered Tau session id and
agent id. Registration waits up to 30 seconds for the initial XMPP online state, joins
the room, sends an XEP-0045 mediated invite plus a direct fallback notice to the
default recipient, and leaves the room with unavailable presence on unregister
or session shutdown. Room identity must be injective for registered routing
keys after XMPP JID normalization: the room localpart uses lowercase hex
encodings of the full Tau session id and full validated `AgentId`, not raw,
hashed, or truncated display text. Once a MUC join succeeds, the worker records
enough room/nick state to send unavailable leave presence on timeout, dropped
registration response, unregister, or shutdown. Direct full-resource chat is
available as a portability fallback and accepts inbound messages only when the
stanza `to` exactly matches the current bound full JID. If reconnect binds a
different resource,
direct-resource registrations are updated and the default recipient is notified
of the new full JID. Existing MUC history is not requested on join, and
delayed/history stanzas are dropped if a server sends them anyway. The MVP does
not submit room configuration forms or affiliation IQs; deployments must use
Prosody/server defaults or preconfiguration for private, hidden, and
members-only room policy.

The worker is the source of truth for XMPP connection readiness: an authenticated
tokio-xmpp `Online` event sets the current bound full JID, and `Disconnected`
clears it plus connection-scoped MUC occupant real-JID cache. `xmpp_register`
waits up to 30 seconds for readiness after starting the bridge before issuing
registration. `xmpp_send` waits up to 30 seconds only after the bridge has
already been started; startup remains registration-driven, and if no registered
conversation exists after readiness the tool still fails with the explicit
`xmpp_register(enabled: true)` requirement.

Incoming XMPP text is emitted as `extension.prompt_submit_request`. The harness
validates the target loaded agent and owns the durable prompt fact.
