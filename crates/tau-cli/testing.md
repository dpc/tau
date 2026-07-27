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
harness-owned prompt commands, and unknown leading-colon fallback.
Dynamic action tests apply successive owner-stamped schema generations and
verify that deep argument completions plus parse errors use the latest
suggestions while stale candidates disappear.

Prompt-history tests cover ordered length-prefixed round trips, malformed or
unsupported records, torn or oversized tails, and command-layer redaction and
routing. Witness tests must separately prove zero-prefix warm validation,
delta-only validation after a cooperative external append, and full bounded
fallback after replacement, same-inode truncation, or boundary mismatch. They
also exercise the production store append rather than only the scanner helper.
They do not require interactive terminal E2E checks.

Developer prompt and tool-preview startup regressions execute the bundled `tau`
binary with isolated home and working directories. They assert observable rendered
contributions rather than only parsed overrides or child-command construction.

Renderer transitions are checked at flush-delimited virtual-terminal frames with
bounded waits. Tests do not request a post-operation `redraw_sync`, which could
hide an incoherent first frame; race regressions may use a deterministic midpoint
hook.

Event-wiring regressions lock historical and live selector sets, payload
dependencies, and catch-up/lifecycle orderings. The chat UI receives
`tool.started` live but intentionally omits it from append-only restore history,
where replaying a completed call's start would transiently resurrect it as
pending before its durable terminal result arrives. Chat does not select
`tool.request` historically or live because its renderer does not consume that
fact; generic and non-chat subscribers can still select it. These exceptions
refine the exact-by-default policy in
[`GATE-exact-event-subscriptions`](../../specs/GATE-exact-event-subscriptions.md).
Cross-crate boundaries pair harness subscription/catch-up coverage with CLI
renderer ordering coverage using the same protocol event shapes.

Agent-roster verification is layered: tau-core owns bounded first-record and
journal-bound checkpoint enrichment; tau-harness owns bounded current/history
lifecycle projection plus requester-directed UI RPC behavior; tau-cli owns
additive filters, deterministic topology, and exact ten-column escaped TSV; and
tau-cli-term owns fixed fzf arguments, statuses, and output. Input-loop coverage
should simulate changes between initial and revalidation snapshots and prove both
successful switching and no-retarget failure paths.

Running-session-list verification is also layered: tau-proto locks the directed
two-field control response; tau-harness tests canonical startup-root ownership,
requester direction, bounded complete discovery, stale/unresponsive exclusion,
sorting, and duplicate retention; tau-cli tests argument canonicalization,
exact filtering, human escaping/deduplication, and JSON rendering. Bundled
`tau` binary tests use isolated runtime directories to lock command dispatch,
stdout and exit status, relative-directory filtered JSON, runtime-I/O failure
without partial output, and inspection-only empty-runtime behavior.
