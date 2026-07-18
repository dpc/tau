# DECISION-tau-ext-xmpp-direct-resource-fallback: Direct-resource fallback scope

Authority: unconfirmed

Direct-resource routing is a fallback. It supports one registered direct agent per
extension instance because one bound full JID cannot distinguish multiple agents for
inbound direct messages. The second-registration error explicitly points users to
`routing.mode: muc` for multiple agents or separate conversations. On reconnect, a
changed bound resource is announced to the default recipient.
