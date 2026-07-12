Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-harness

- Do not drop, downgrade, or make startup-only any extension `HarnessInputMessage::ConfigError`. The harness must convert it into mandatory `harness.notice` visible in the UI.
- mandatory harness diagnostics, especially config parse errors, must be replayed to late UI subscribers. Daemon startup commonly finishes extension configuration before the terminal UI subscribes, so live-only publication is insufficient.
- Read the applicable `specs/DESIGN-*.md` records before changing lifecycle/startup behavior, prompt assembly, system prompt templating, or adding harness tests; they record focused design decisions for this crate.

- Read `specs/ARCH-tau-harness.md` before changing or reviewing harness event sequencing, persistence, interception, extension boundaries, session semantics, or extension-data behavior.
- Read the applicable trust-boundary records under `specs/` before changing daemon IPC, listener lifecycle, shutdown, runtime discovery, or security-sensitive harness behavior.
