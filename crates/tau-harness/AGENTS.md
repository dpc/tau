# tau-harness

- Do not drop, downgrade, or make startup-only any extension `HarnessInputMessage::ConfigError`. The harness must convert it into mandatory `harness.notice` visible in the UI.
- mandatory harness diagnostics, especially config parse errors, must be replayed to late UI subscribers. Daemon startup commonly finishes extension configuration before the terminal UI subscribes, so live-only publication is insufficient.
- Read `design.md` before changing lifecycle/startup behavior or adding harness tests; it records focused testing expectations for state-machine and replay contracts.

- Read `ARCHITECTURE.md` before changing or reviewing harness event sequencing, persistence, interception, extension boundaries, session semantics, or extension-data behavior.
