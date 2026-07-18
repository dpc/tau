Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-xmpp

This extension bridges untrusted external XMPP text into Tau. Before changing routing, configuration, secrets, connection lifecycle, or tool behavior, read `specs/ARCH-tau-ext-xmpp.md`, the applicable local Linked Specs under `specs/`, and `../../specs/ARCH-external-message-boundary.md`.

Keep configuration keys snake_case and reject unknown fields. Never log XMPP
passwords or private message bodies unless the surrounding code already treats
them as user-visible prompt text.

See the applicable Linked Specs under `specs/` for MVP design decisions and documented limitations. See
`testing.md` for unit-test expectations and the live Prosody smoke-test path.
