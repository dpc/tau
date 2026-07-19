# Testing tau-cli

`dev_tmux` provider-access tests stay focused on config parsing, exact allowlist
copying, stale scratch reconciliation, warnings, and refusal of symlink,
non-regular, path-traversal, or unsafe entries.

Pure transcript-renderer tests use representative fixture themes with distinct
semantic attributes. They assert text preservation except for documented
display-only transforms and verify semantic styling on resolved spans. Built-in
theme tests only check parsing and intentional theme-level invariants.

Input-loop routing tests cover emitted notices and harness events or prompts,
including the shared surface syntax of CLI commands, extension actions,
harness-owned prompt commands, and unknown leading-slash fallback.

Prompt-history tests cover ordered length-prefixed round trips, malformed or
unsupported records, torn or oversized tails, and command-layer redaction and
routing. They do not require interactive terminal E2E checks.

Developer prompt and tool-preview startup regressions execute the bundled `tau`
binary with isolated home and working directories. They assert observable rendered
contributions rather than only parsed overrides or child-command construction.

Renderer transitions are checked at flush-delimited virtual-terminal frames with
bounded waits. Tests do not request a post-operation `redraw_sync`, which could
hide an incoherent first frame; race regressions may use a deterministic midpoint
hook.

Event-wiring regressions lock historical and live selector sets, payload
dependencies, and catch-up/lifecycle orderings. The chat UI receives
`tool.request` and `tool.started` live but intentionally omits them from
append-only restore history, where replaying a completed call's start would
transiently resurrect it as pending before its durable terminal result arrives.
This exception refines the exact-by-default policy in
[`DECISION-exact-event-subscriptions`](../../specs/DECISION-exact-event-subscriptions.md).
Cross-crate boundaries pair harness subscription/catch-up coverage with CLI
renderer ordering coverage using the same protocol event shapes.
