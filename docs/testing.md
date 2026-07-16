# Testing guidelines

## Rendering themes

Rendering and theme behavior tests should use artificial fixture themes with
explicit semantic attributes. Do not snapshot or assert details of Tau's built-in
themes from renderer tests; built-ins are product defaults and may change for
readability without implying renderer behavior changed.

Built-in theme tests should be limited to parsing and intentional invariants of
those built-ins, such as the conservative default theme staying within its
allowed safe foreground colors and avoiding background colors.

## Terminal screen renderer boundaries

Terminal screen renderer tests should protect observable terminal behavior at
the boundaries where the in-memory screen model meets terminal scrolling. Prefer
focused `tau-term-screen` unit tests backed by `vt100::Parser` or the local
pending-wrap test model so assertions cover visible rows, scrollback order,
cursor position, exact-width pending-wrap transitions, shrink clearing, and
styled-cell output rather than only inspecting emitted escape bytes.

When refactoring renderer internals, keep the behavior-preserving contract
explicit: changed-range detection uses absolute content line indices, rows above
the previous viewport are treated as existing scrollback, missing new rows still
matter when old on-screen rows disappeared, and downward movement must continue
to scroll naturally at the bottom edge. Add regression tests for any newly found
terminal edge case instead of relaxing cargo-crap thresholds or accepting
snapshot-only coverage.

## Manual Tau terminal E2E checks

Use `tau dev tmux` for agent-controlled manual checks of the real terminal UI
when behavior is too interactive for focused unit tests. The helper starts Tau in
a private tmux server with scratch `HOME`/XDG state, disables extensions by
default, and enables `core-shell`. It remains local-only unless
`~/.config/tau/testing.yaml` explicitly allowlists provider profile names under
`testing_providers`. When that file is absent or the list is empty, start prints
a warning and copies no real provider credentials/config/state.

Discover provider profile names in the real Tau environment with
`tau provider list`, then use the exact displayed profile name in
`testing_providers`. The name must match the stem of
`~/.local/state/tau/auth.d/<provider>.json`.

When providers are allowlisted, the helper copies only exact
`~/.local/state/tau/auth.d/<provider>.json` files into scratch state and enables
`provider-builtin` for the child Tau. It does not copy all providers, lock files,
general config, sessions, logs, or unrelated state.

This workflow complements automated tests; it is not a replacement for focused
regression coverage. Reusable steps live in
`.agents/skills/tau-e2e-testing-tmux/SKILL.md`.


## Provider response streaming tests

### Deterministic full-harness provider acceptance

`tau-e2e-tests` has an always-on `DeterministicFixture` that launches the
test-only `tau-e2e-fake-provider` by exact path through normal extension
supervision. Its strict inline `ScenarioV1` drives synthetic streaming and a
real deterministic `tau-ext-test-dummy` tool continuation. `ScenarioV2` adds
bounded exact-correlation lanes for typed failures, cancellation/timeout and
same-agent post-cancel liveness,
barriers, fatal disconnect, quiescent same-agent restore, and one closed
`restart_test_dummy` call/result pair. Embedded and
test-only daemon paths require no credentials, network, shell, sleeps, or VCR
gate. Panics, `run_turn` failures, and daemon exits before exact-consumption
acknowledgement retain the private generated configuration, scenario, durable
event log, extension stderr, and bounded semantic provider trace.

The Unix-only `core_resume` target additionally launches the exact universal
Tau binary under a fixed PTY for a fresh boot and explicit
`tau -r <session-id>` boot. Its VT model checks the completed dummy row is always
terminal throughout Boot B historical restoration and the fresh resumed turn;
Boot A is allowed to show the ordinary live pending state before completion.
A side UI observer preserves replay metadata and typed CBOR
store reads prove identity and prefix/suffix durability. This is one narrow
known-bug terminal projection gate, not broad rendering fidelity. The lane does
not validate provider-builtin, upstream ChatGPT lowering/parsing, WebSocket
behavior, production retries, or crash-exact replay.

The separate `core_shell_resume` target starts that same universal binary as
bundled `component ext-shell`. Its closed scenario exposes only `workdir` and
`edit`, commits a canonical per-agent workdir, replaces the daemon and extension,
and performs a relative context-checked edit on the same durable agent. Scratch
bytes and an outside canary are safety oracles; locks stay disabled and this is
not a filesystem sandbox claim.
Unlike the PTY gate, the headless daemon has no post-EndTurn UI-idle frame.
Gate 2 therefore defines quiescence as an exact correlated terminal response
with complete durable tool facts followed by verified disappearance of its
owned daemon/extension process group and Unix socket.

The independent `cancellation_liveness` target is mandatory Gate 3. It uses two
bounded provider lanes to prove exact cancellation isolation, one transient
terminal per held prompt, no accepted or durable late terminal, and a fresh
successful prompt on the second selected agent's same lane. Its warm-process
quiescence boundary is that final EndTurn plus exact durable facts and reaped
process group/socket; it makes no PTY, core-shell, or crash-exact cancellation
persistence claim.

`ci.deterministicE2eTests` is a mandatory selfci derivation. Its exact target
plus `--no-tests=fail` prevents silent filtering, and the Nix build sandbox
denies network access independently of the fixture implementation.
Focused fake-provider unit tests own strict Configure grammar, resource bounds,
diagnostic truncation, malformed/mismatched checkpoint rejection, binding
uniqueness, cursor bounds, and barrier structure; the subprocess E2Es own representative lifecycle,
routing, streaming, mismatch, tool continuation, exact cancellation, bounded
timeout, concurrent isolation, clean restore, fatal disconnect, persistence,
and shutdown.

Tests for `provider.response_updated` should use append-delta semantics: multi-update assistant/reasoning cases send only the newly appended suffix in each update. Do not feed full accumulated snapshots through delta helpers unless the test is explicitly checking legacy/invalid payload handling. Final-response tests should continue to assert complete `provider.response_finished.output_items`.

Provider streaming tests must also assert response/progress rate-limit boundaries: non-terminal `provider.response_updated` frames for one prompt publish the first non-empty streamed output sample promptly, suppress empty zero-byte idle samples until the first one-second deadline, batch later non-terminal samples to at most one per second, allow stats-only/no-byte samples as valid liveness updates once due, preserve suppressed visible deltas, and allow a terminal flush to publish the final batched suffix immediately before `provider.response_finished`. Previous/current response-stat assertions should prove that `previous` equals the last emitted provider sample, not an internal suppressed calculation. Tests should also prove response bytes are recorded at the backend transport receive boundary before semantic parsing.

Harness tests for provider response stats should assert only validation and pass-through: wrong-owner provider updates are rejected, accepted stats-only `provider.response_updated` events are broadcast to subscribers, `agent_id` is rewritten from prompt ownership, and no harness-owned response-throughput projection is emitted.

Provider retry scheduler tests should use injected/fake time and deterministic
jitter rather than real multi-minute sleeps. Cover retry-to-park handoff,
released worker capacity, due/fresh fairness, shared-cooldown extension,
prompt-scoped and global cancellation in queued/delayed/active states,
profile reload before a later success, and exactly one submitted/terminal
lifecycle. Backend fixtures should cover Responses HTTP/SSE/WebSocket, generic
Chat Completions, and OpenRouter retry-then-success paths, including tentative
output clearing and trusted hint lower bounds.
Manual `/retry` coverage must drive the scheduler/runtime boundary
deterministically: assert delayed-count transfer, timer/manual and
shutdown/manual ownership races, one-shot cooldown bypass, and that a failed
manually released attempt increments accounting once and parks again normally.
Successful-probe coverage must also prove exact attempt/profile/generation
validation, peer and chained-continuation wakeup, independent-deadline and
cross-provider isolation, stale/error/cancel negatives, identity rotation, and
that quota telemetry alone cannot release inference cooldowns.
The synchronous scheduler state additionally has a bounded reference-model
gate. Every PR replays fixed seeds over scheduler-owned schedule, extend,
exact-generation release, manual retry, targeted/global cancellation, virtual
advance, and duplicate AP commands. Production-runtime fixtures separately own
identity rotation, telemetry non-authority, shutdown/EOF, cancellation/commit,
and provider-disconnect coverage rather than simulating those transitions in
the queue model. Failures must include the seed and proptest-minimized command
trace. Scheduled CI can increase the deterministic budget with
`TAU_SCHEDULER_MODEL_CASES`; the ordinary default must remain fast, sleep-free,
and reliable.
Boundary acceptance should keep representative wire contracts in their owning
adapters: local Responses HTTP/SSE and WebSocket failures plus generic Chat
Completions/OpenRouter failures must traverse production parsing and typed retry
classification. The joined incident gate may then consume that typed decision;
the feature-gated causal fixture carries its manually retried probe into an
embedded deterministic tool round, then feeds those exact committed events
through renderer state to assert main, global, and watched
activity become idle. These gates use loopback peers and explicit completion
signals only—never provider auth, Internet access, model prose, tmux, or sleeps.

The focused ChatGPT WebSocket lane stays inside `tau-provider-chatgpt`.
Loopback-only finite peers exercise production request lowering and frame
parsing together with pool reuse, reconnect, cooperative cancellation,
provider-frame deadlines, typed errors, and the no-HTTP-fallback commitment.
Peers use synthetic credentials, explicit request/completion signals, bounded
scripts and socket deadlines, and joined teardown. Provider-builtin retry and
cooldown tests remain separate on its injected executor and virtual monotonic
clock. Do not add a harness→provider-builtin→local-WebSocket gate, backend
resolver, user OAuth URL override, or common scenario language merely to join
these layers; the deterministic fake provider does not cover upstream ChatGPT
transport contracts.

## OAuth response safety

Shared OAuth protocol regressions live in `tau-provider`: bounded response
reads, flat and nested error envelopes, malformed responses, typed field bounds,
and credential-safe `Display`/`Debug` formatting. Parser and formatting cases use
synthetic in-process values; HTTP read/classification cases use loopback servers.
Live provider auth, Internet access, and real credentials are intentionally
excluded. Provider-builtin changes that log typed OAuth failures should cover the
consumer integration boundary without placing credentials or raw OAuth bodies
in fixtures.

Provider-builtin refresh tests use temporary auth files and injected endpoint
outcomes to cover per-process exact-generation suppression, profile changes,
authoritative locked-generation handoff, and valid-only fallback. They exclude
live provider auth, Internet access, real credentials, and wall-clock sleeps.

## Curated provider VCR compatibility evidence

The small corpus under
`crates/tau-provider-chatgpt/fixtures/provider-vcr/` is synthetic,
structurally sanitized evidence that representative Responses SSE and
WebSocket wire frames still traverse the production parsers. It is not a
scheduler, retry-timing, concurrency, or model-output oracle; deterministic
runtime and local scripted-transport tests own those contracts.
Persisted transcript replay is likewise reconstruction evidence, not live
transport execution or a substitute for the focused localhost lane.
Its durable ownership and scope are recorded in
[`DESIGN-tau-provider-chatgpt-curated-vcr`](../crates/tau-provider-chatgpt/specs/DESIGN-tau-provider-chatgpt-curated-vcr.md):
the provider crate owns the corpus, manifest/audit, parser facts, and refresh
review, while `tau-vcr` owns only generic storage.

The dedicated `ci.vcrTests` Nix derivation forces `TAU_VCR=replay-only`, has no
live fallback, fails when its exact test filter runs zero tests, and runs in the
network-denied Nix build sandbox. Its strict manifest and fixture audit checks
schema and redaction versions, unique keys, declared transport/outcome,
cassette/event/frame/delta limits, complete terminal consumption, forbidden
secret categories, suspicious long tokens, and host paths. Provider replay
validates recorded deltas but intentionally ignores them during parser-only
functional replay.

The explicitly versioned multi-attempt/AP-lane schema proposed for quota
testing is deliberately not implemented. Its status/header/disconnect model,
sanitized request projection, and attempt orchestration would duplicate the
production local-server and scheduler gates while creating a second evolving
semantic format. Reconsider only when a concrete provider wire regression
cannot be represented by the successful single-attempt corpus or the existing
local transport fixtures. At that point use a new attempt-sequence schema in
full—never incrementally turn this success-stream schema into scheduler
authority.

Refresh is deliberate and review-only:

1. Create a new key; cassette writes are atomic, private, exclusive, and never
   overwrite an existing file. Never enable recording in CI.
2. Generate only synthetic requests and responses. Do not copy a raw live
   capture into the repository. Request projections may contain only stable
   shape facts, never prompts, credentials, IDs, reasoning, tool output, or
   paths.
3. Add the declaration to `manifest.yaml`, run `nix build -L .#ci.vcrTests`,
   and inspect the full diff. Record synthetic provenance, pinned
   surface/transport, and compatibility intent in `manifest.yaml`; explain in
   the change description why the new evidence is valuable.
4. A reviewer independently confirms structural sanitization, public-safe
   classification, bounded data, and that the fixture adds wire-compatibility
   value rather than duplicating scheduler tests. Unavoidable real-provider
   captures remain private with separate access and retention controls.

CLI coverage must separately prove static completion, exact argument-free
parsing without prompt resubmission, and requester-visible result rendering.

## Provider stream repetition guard

When changing provider streaming parsers, add focused tests for assistant text, reasoning text, and tool-argument deltas. Tests should include high-volume exact loops that abort and negative cases for short repeated words, repeated prefixes with changing payloads, and line blocks below threshold.

Responses-style parsers must also cover final snapshot/done events (for example
`response.output_text.done`, tool argument/input done events, and
`response.output_item.done`) because providers can send complete content there
without earlier deltas.


## Skill discovery and loading

`tau-skills` tests should cover frontmatter parsing, validation helper
contracts, deterministic directory discovery, bounded discovery reads,
symlink-following for roots/directories/Markdown skill files, canonical-directory
cycle prevention, collision winner selection, scoped prompt defaults, and built-in
self-knowledge skills. Prefer focused fixtures that exercise one contract at a
time, including oversized bodies/frontmatter and UTF-8-safe truncation edge
cases.
