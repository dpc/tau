Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-socket agent notes

Before changing this crate, read:

- `specs/ARCH-tau-socket.md` for listener/client directionality and ownership boundaries.
- the applicable trust-boundary records under `specs/` for local IPC and socket path cleanup invariants.

Keep changes focused on Unix socket transport behavior. Preserve explicit receive
outcomes, partial-frame decode errors, safe stale-socket cleanup, and background
reader shutdown behavior. Update focused regression tests when these contracts change.
