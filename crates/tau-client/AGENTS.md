Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-client

This crate contains shared client-side runtime helpers for Tau extension and UI
protocol peers. Keep public APIs conservative and document protocol guarantees
in rustdoc because downstream extension crates are expected to build on them.

When changing this crate:

- preserve the existing `tau-proto` wire format;
- keep startup ordering stable (`Hello`, initial `Configure` scope/state/handlers,
  static declarations, accepted Configure-derived declarations, `Ready`);
- add focused unit tests for new handler or protocol lifecycle behavior;
- update `specs/ARCH-tau-client.md` when changing lifecycle, replay, writer-thread,
  config, or intercept semantics.
- Harness-extension interface changes are governed by
  `../../specs/DESIGN-persistence-and-extension-interface-change-approval.md`;
  obtain the required standalone design approval before functional changes.
