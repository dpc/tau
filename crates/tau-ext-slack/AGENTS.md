Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-slack

This extension bridges untrusted external Slack text into Tau. Before changing routing, configuration, secrets, connection lifecycle, or tool behavior, read `specs/ARCH-tau-ext-slack.md`, the applicable local `specs/DESIGN-*.md` records, and `../../specs/ARCH-external-message-boundary.md`.

Keep configuration keys snake_case and reject unknown fields. Never log Slack
app tokens, bot tokens, Socket Mode websocket URLs, or private message bodies
unless the surrounding code already treats the text as user-visible prompt text.

After user-visible capability, configuration, Slack scope/event, or operational
changes, update the built-in `tau-self-knowledge-ext-slack` skill.
