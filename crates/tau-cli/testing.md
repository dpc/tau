# Testing tau-cli

`dev_tmux` provider-access tests stay focused on config parsing, exact allowlist
copying, stale scratch reconciliation, warnings, and refusal of symlink,
non-regular, path-traversal, or unsafe entries.

Pure transcript-renderer tests use representative fixture themes with distinct
semantic attributes. They assert text preservation except for documented
display-only transforms and verify semantic styling on resolved spans. Built-in
theme tests only check parsing and intentional theme-level invariants.

## Markdown renderer fuzz properties

The bounded Markdown renderer properties use the existing `proptest` framework.
They run 64 arbitrary Unicode cases and 64 delimiter-heavy cases in ordinary CI,
covering static rendering plus every UTF-8-safe append-only streaming snapshot.
They require the sealed streaming spans to match static rendering exactly before
one styled live progress marker, so malformed input cannot panic or alter
finalized content.

The ignored `markdown_heavy_fuzz_harness` compiles with the normal test binary but
does not run in `cargo nextest` or selfci, keeping CI bounded. It defaults to 1,000
larger generated cases. Run a deeper local workload deliberately:

```sh
TAU_MARKDOWN_FUZZ_CASES=20000 \
  cargo test -p tau-cli markdown_heavy_fuzz_harness -- --ignored
```

Input-loop routing tests cover emitted notices and harness events or prompts,
including the shared surface syntax of CLI commands, extension actions,
harness-owned prompt commands, and unknown leading-colon fallback.
Dynamic action tests apply successive owner-stamped schema generations and
verify that deep argument completions plus parse errors use the latest
suggestions while stale candidates disappear.

Terminal foreground-restoration fail-stop coverage is intentionally layered.
`tau-cli-term::bounded_command` owns checked settlement for captured and
inherited-stdio children, preservation of primary and restoration failures,
bounded cleanup, and process-group termination with bounded non-runnable
descendant cleanup. The high-level `tau-cli-term` guard tests own the no-resume
decision for prompt commands and the picker's distinct explicit-resume path.
`tau-cli` tests own attachment-fatal routing through authorized daemon-preserving
UI disconnect and daemon disposition. `tau-cli-term-raw` tests own paused
shutdown, Drop cleanup suppression, and the no-redraw-write sink oracle. Changes
to any layer must run the focused tests in all four owners rather than replacing
them with a broad subprocess matrix.

Prompt-history tests cover ordered asynchronous-worker persistence, nonblocking
item/byte admission drops, unavailable-worker drops, malformed or unsupported
records, torn or oversized tails, and command-layer redaction and routing.
Witness tests must separately prove zero-prefix warm validation, delta-only
validation after a cooperative external append, and full bounded fallback after
replacement, same-inode truncation, or boundary mismatch. Tests use a
test-only worker barrier to observe accepted writes; they deliberately do not
test shutdown draining because production never joins or drains that
best-effort worker. They do not require interactive terminal E2E checks.

Gmail OAuth finish redaction uses layered ownership oracles with distinct
code/state sentinels. `tau-cli-term-raw` and `tau-cli-term` own recalled-source
navigation, undo/redo, search-row, preview, and selection safety; `tau-cli` owns
sensitive command classification, editor context, persistent history, dynamic
action parsing, content-free/contentful drafts, literal escapes, and exact raw
action construction. Harness action tests own exact-provider routing and
requester-only results, debug-log tests own complete serialized publication
absence, and PIM tests own OAuth exchange/result sanitization. A change to this
exception must run the focused owners together; helper-only classifier coverage
cannot replace production input-loop and wire assertions.

Developer prompt and tool-preview startup regressions execute the bundled `tau`
binary with isolated home and working directories. They assert observable rendered
contributions rather than only parsed overrides or child-command construction.

Papercut tests serialize records through `tau_ext_utils::PapercutRecord`, the
same contract used by the reporter, then exercise the CLI reader's bounded,
no-follow storage path. They cover plain and Markdown rendering, empty and
repeated clear, rejected malformed/unsupported/unsafe storage, and a
test-only post-delete midpoint that proves a real `clear()` preserves a waiting
reporter append after its shared-lock boundary.

Renderer transitions are checked at flush-delimited virtual-terminal frames with
bounded waits. Tests do not request a post-operation `redraw_sync`, which could
hide an incoherent first frame; race regressions may use a deterministic midpoint
hook.

Event-wiring regressions lock historical and live selector sets, payload
dependencies, and catch-up/lifecycle orderings. The chat UI receives
`tool.started` both live and from append-only restore history. During cold attach
it folds durable starts against canonical terminals through
`session.replay_complete`, retains only current-session loaded-agent starts joined
to provider-declared calls in that agent's replayed transcript, and places buffered
live starts and progress after that baseline only while their lifecycle owner
is materialized, stopping at the first terminal. The first terminal remains
visible even without a materialized start. A buffered pre-terminal progress
frame keeps its authorized replay start temporarily so its terminal can remove
the live row; later starts or progress frames are suppressed.
`tool.request` remains excluded
historically and live because request admission does not establish a dispatched
pending lifecycle and the renderer does not consume it. Generic and non-chat
subscribers can still select requests. These choices refine the exact-by-default
policy in
[`GATE-exact-event-subscriptions`](../../specs/GATE-exact-event-subscriptions.md).
Cross-crate boundaries pair harness subscription/catch-up coverage with CLI
renderer ordering coverage using the same protocol event shapes.

Agent-roster verification is layered: tau-core owns bounded first-record and
journal-bound checkpoint enrichment; tau-harness owns bounded current/history
lifecycle projection plus requester-directed UI RPC behavior; tau-cli owns
additive filters, deterministic topology, and exact eleven-column escaped TSV; and
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
