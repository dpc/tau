# tau-ext-slack

This extension bridges untrusted external Slack text into Tau. Before changing
routing, config, secrets, Socket Mode lifecycle, or tool behavior, read
`ARCHITECTURE.md`, `design.md`, and `SECURITY.md` in this crate, plus the workspace
`SECURITY.md`.

Keep configuration keys snake_case and reject unknown fields. Never log Slack
app tokens, bot tokens, Socket Mode websocket URLs, or private message bodies
unless the surrounding code already treats the text as user-visible prompt text.
