Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-harness

- Read the applicable `specs/DESIGN-*.md` records before changing lifecycle/startup behavior, prompt assembly, system prompt templating, or adding harness tests; they record focused design decisions for this crate.

- Read `specs/ARCH-tau-harness.md` before changing or reviewing harness event sequencing, persistence, interception, extension boundaries, session semantics, or extension-data behavior.
- Read the applicable trust-boundary records under `specs/` before changing daemon IPC, listener lifecycle, shutdown, runtime discovery, or security-sensitive harness behavior.
- Event-log/journal and harness-extension interface changes are governed by
  `../../specs/DESIGN-persistence-and-extension-interface-change-approval.md`;
  obtain the required standalone design approval before functional changes.
- For IPC/resource review, state whether the path is trusted configured-extension
  IPC, cooperative inter-harness IPC, or genuinely untrusted external ingress.
  Start at [`../../SECURITY.md`](../../SECURITY.md), then
  `specs/SPEC-tau-harness-extension-lifecycle.md`,
  `specs/SPEC-tau-harness-session-state.md`, and
  `specs/ARCH-tau-harness.md`. If wording appears to conflict, ask rather than
  inferring a stronger threat model.
