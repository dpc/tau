# tau-ext-xmpp

This extension bridges untrusted external XMPP text into Tau. Before changing
routing, config, secrets, connection lifecycle, or tool behavior, read
`ARCHITECTURE.md` and `SECURITY.md` in this crate, plus the workspace
`SECURITY.md`.

Keep configuration keys snake_case and reject unknown fields. Never log XMPP
passwords or private message bodies unless the surrounding code already treats
them as user-visible prompt text.

See `design.md` for MVP design decisions and documented limitations. See
`testing.md` for unit-test expectations and the live Prosody smoke-test path.
