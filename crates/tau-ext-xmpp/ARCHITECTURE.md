# tau-ext-xmpp architecture

`std-xmpp` is a disabled-by-default personal XMPP text bridge. It exposes only
`xmpp_register` and `xmpp_send`; the model never supplies arbitrary destination
JIDs. The extension does not connect to XMPP until an agent registers.

A single XMPP account may be used by multiple Tau processes concurrently because
this extension always requests a generated, high-entropy resource and then uses
the server-returned bound full JID.

The preferred routing mode is one MUC room per registered agent/session
conversation. Direct full-resource chat is available as a portability fallback
and accepts inbound messages only when the stanza `to` exactly matches the
current bound full JID. If reconnect binds a different resource, direct-resource
registrations are updated and the default recipient is notified of the new full
JID. Existing MUC history is not requested on join, and delayed/history stanzas
are dropped if a server sends them anyway. The MVP does not submit room
configuration forms or affiliation IQs; deployments must use Prosody/server
defaults or preconfiguration for private, hidden, and members-only room policy.

Incoming XMPP text is emitted as `extension.prompt_submit_request`. The harness
validates the target loaded agent and owns the durable prompt fact.
