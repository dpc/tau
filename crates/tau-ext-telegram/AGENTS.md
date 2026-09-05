Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-telegram

This extension bridges untrusted external Telegram text into Tau. Before changing routing, configuration, secrets, connection lifecycle, or tool behavior, read `specs/ARCH-tau-ext-telegram.md`, the applicable local Linked Specs under `specs/`, and `../../specs/ARCH-external-message-boundary.md`.

Follow the hermetic test strategy in [testing.md](testing.md).

- [`SECURITY.md`](SECURITY.md) — required reading for Telegram trust,
  correlation, retry, replay, recovery, and lifecycle boundaries.

Keep configuration keys snake_case and reject unknown fields. Never log bot
tokens or Telegram message content unless the surrounding code already treats it
as user-visible prompt text.

- Before introducing a new free-form payload kind, or reframing an existing one, in the shared generic user-role `ContentPart::Text` carrier, read [`GATE-new-generic-user-payload-envelopes`](../../specs/GATE-new-generic-user-payload-envelopes.md) and [`SPEC-exact-sentinel-prompt-envelopes`](../../specs/SPEC-exact-sentinel-prompt-envelopes.md); use the shared registry rather than a component-local provenance wrapper. Typed tool results and the system/developer prompt channel are outside that rule.
