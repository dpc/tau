Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-telegram

This extension bridges untrusted external Telegram text into Tau. Before changing routing, configuration, secrets, connection lifecycle, or tool behavior, read `specs/ARCH-tau-ext-telegram.md`, the applicable local Linked Specs under `specs/`, and `../../specs/ARCH-external-message-boundary.md`.

Follow the hermetic test strategy in [testing.md](testing.md).

Keep configuration keys snake_case and reject unknown fields. Never log bot
tokens or Telegram message content unless the surrounding code already treats it
as user-visible prompt text.
