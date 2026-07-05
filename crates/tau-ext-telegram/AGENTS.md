# tau-ext-telegram

This extension bridges untrusted external Telegram input into Tau. Before
changing routing, config, secrets, poller lifecycle, or tool behavior, read
`ARCHITECTURE.md`, `design.md`, and `SECURITY.md` in this crate, plus the
workspace `SECURITY.md`.

Keep configuration keys snake_case and reject unknown fields. Never log bot
tokens or Telegram message content unless the surrounding code already treats it
as user-visible prompt text.
