# DECISION-tau-ext-xmpp-opt-in-bridge: Disabled opt-in personal bridge

Authority: unconfirmed

`std-xmpp` is disabled by default and publishes only opt-in tools. Agents must call
`xmpp_register(enabled: true)` before `xmpp_send` works.

Once an agent registration starts the XMPP worker, later `Configure` messages are
rejected with `ConfigError` instead of partially applying new settings to only the
tool-side state. Restart Tau to apply changed XMPP credentials, allowlists, or routing
settings.
