# Testing guidelines

## Durable Rostra notification worker

Test `std-rostra` notification changes at three boundaries. State fault tests
cover atomic policy/checkpoint replacement, restart reconstruction, poisoning,
and report-ID allocation. State-machine and report tests cover batching,
projection, timing, canonical acknowledgement, and bounded payload assembly.
Worker tests cover report-enqueue backoff, historical-boundary selection, and
the exact one-row argument at the production database scan seam. Keep that
behavioral call-boundary oracle when changing the scan.
Add deterministic feed/follow-snapshot fixtures when changing source selection;
do not rely on a live Rostra broadcast because it is only a lossy wake hint.


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
`~/.config/tau/testing.yaml` explicitly allowlists extension/provider pairs under
`testing_providers`. When that file is absent or the list is empty, start prints
a warning and copies no real provider credentials/config/state.

Discover provider profile names with `tau provider list`, then name both the
configured extension instance and exact provider:

```yaml
testing_providers:
  - extension: provider-builtin
    provider: chatgpt
```

When providers are allowlisted, the helper copies only exact
`providers/<extension>/<provider>.json` settings and
`secrets/ext/<extension>/providers/<credential-id>/` typed credentials into scratch
state. It does not copy all providers, general config, sessions, logs, or
unrelated state.

This workflow complements automated tests; it is not a replacement for focused
regression coverage. Reusable steps live in
`.agents/skills/tau-e2e-testing-tmux/SKILL.md`.

## Agent-message ownership tests

Keep message tests separated by authority layer:

- protocol/store tests prove one typed sent or received journal occurrence and
  exact durable sequence;
- core fold tests prove branch placement, tree-global tool-round adjacency,
  replay equivalence, and canonical provider rendering;
- harness tests prove payload-free live wakes, checkpoint acknowledgement,
  wait interruption, navigation dormancy/reselection, admission release, and
  pre-persistence rejection;
- deterministic E2E tests prove canonical watch, authenticated-peer context,
  and the production `message` tool. The message-tool gate uses a
  test-driver-created idle worker, then requires dual journal ownership, one
  payload-free worker activation, one canonical inbound wrapper, a compact
  sender result, and fresh explicit turns for both agents.

Do not use body or logical message-ID searches as occurrence or wake oracles.
Correlate typed events by owning journal sequence, branch node, checkpoint, and
activation class. See
[`SPEC-agent-message-delivery`](../specs/SPEC-agent-message-delivery.md) for the
end-to-end contract.

### Warm-process multi-agent smoke

Use this opt-in smoke only with a trusted provider profile explicitly allowlisted
in `~/.config/tau/testing.yaml`. It exercises the current process; it is not a
cold-resume test and must not become a credentialed CI gate.

```sh
cargo build -p tau --bin tau
scratch_parent=$(mktemp -d "${TMPDIR:-/tmp}/tau-s8-tmux.XXXXXX")
scratch_root="$scratch_parent/root"
tmux_session=tau-s8-smoke
target/debug/tau dev tmux start \
  --tau-bin target/debug/tau \
  --scratch-root "$scratch_root" \
  --session "$tmux_session"
```

Inside the isolated session, create a main and worker, inspect `:agent`, select
each with `:agent switch <agent-id>`, and send one follow-up. From a separate
shell using that scratch environment, verify the directed external roster with:

```sh
HOME="$scratch_root/home" \
XDG_CONFIG_HOME="$scratch_root/config" \
XDG_STATE_HOME="$scratch_root/state" \
XDG_RUNTIME_DIR="$scratch_root/run" \
target/debug/tau agent list <session-id>
```

Record only agent/session IDs and lifecycle, selection, and completion outcomes;
do not retain provider wording or copied authentication state. The helper cannot
pass `tau resume SESSION` to its child Tau, and its tmux session ends when Tau exits. It therefore
does **not** test manual cold resume. The Unix spawned-PTY `core_resume` gate owns
public-terminal cold-resume acceptance until the helper gains a tested resume
argument boundary.

Always remove the scratch root after the smoke:

```sh
target/debug/tau dev tmux stop \
  --scratch-root "$scratch_root" \
  --session "$tmux_session" \
  --remove-scratch
rmdir -- "$scratch_parent"
```

If Tau has already exited, tmux may have removed the session before `stop` can
run its cleanup path. In that failure case, compare `$scratch_root` with the
exact `scratch root:` printed by `start`, verify it is non-empty and not `/`,
then require the helper's exact non-symlink marker before removing that exact
directory manually:

```sh
marker="$scratch_root/.tau-dev-tmux-scratch"
test -n "$scratch_root" && test "$scratch_root" != / &&
  test -d "$scratch_root" && test ! -L "$scratch_root" &&
  test -f "$marker" && test ! -L "$marker" &&
  printf 'tau dev tmux scratch v1\n' | cmp -s - "$marker" &&
  rm -rf -- "$scratch_root" &&
  rmdir -- "$scratch_parent"
```

## Durable journal append tests

Keep failure-atomic journal tests separated by ownership. `tau-core`'s
`record_log` tests exhaust length-prefix and payload offsets plus rollback
truncation. Agent and session store tests prove canonical-manifest,
derived-checkpoint, sequence-retry, restore-stream, and per-path poison
behavior. Recovery tests prove incomplete EOF tails are truncated, while
complete invalid frames and valid-looking suffixes remain unchanged and fail
closed. Read-only snapshot tests remain strict. Use deterministic injected I/O failures without
sleeps or timing assumptions.

The lifecycle-owned semantic persistence worker owns deterministic
bounded-admission, FIFO frame/checkpoint/touch write, rollback, poison, sync,
directory-debt, blocked-I/O, release, and worker-exit tests. The legacy
`JournalSyncWorker` suite covers only the test-fixture compatibility writer.
Managed-store tests prove admission and fold complete before asynchronous I/O,
while restart tests prove the longest valid durable prefix and strict manifest
authority.

## Provider response streaming tests

### Deterministic full-harness provider acceptance

`tau-e2e-tests` has an always-on `DeterministicFixture` that launches the
test-only `tau-e2e-fake-provider` by exact path through normal extension
supervision. Its strict inline `ScenarioV1` drives synthetic streaming and a
real deterministic `tau-ext-test-dummy` tool continuation. Its closed
current-status sequence also installs the production `status` handler before the
first prompt and checks both parallel orders, accepted/rejected Working, repeated
work while Working, the Working-final challenge, and Done/Blocked release.
`ScenarioV2` adds
bounded exact-correlation lanes for typed failures, one canonical
context-window failure that durably starts reactive opaque compaction and one
opaque-replacement plus retained-overflow-suffix automatic continuation,
cancellation/timeout and
same-agent post-cancel liveness,
barriers, fatal disconnect, quiescent same-agent restore, and one closed
`restart_test_dummy` call/result pair. Its session-restore grammar also has one
exact production `agent_start` pair, a harness-minted child binding, and bounded
typed automatic-watch matching. The corresponding two-agent gate proves a
completed durable worker cold-restores with its own route and transcript while
the daemon-lifetime watch is absent. Its S2 grammar adds one closed
`AgentWatchCall`/`AgentWatchResult` pair, and the fresh fixture proves a new
subscription produces exactly one non-model initial snapshot plus one
initial work-status/prompt/response set under the content-delivery causal edge.
S3 reuses the S1 grammar, creates one promptless ephemeral worker through the UI,
and seeds a typed durable worker load/unload history between clean boots. Its
current/history roster, route-rejection, replay, typed-store, and exact-lane
oracles prove Boot B creates routes only for the current durable pair, preserves
the unloaded worker only in history, and drops ephemeral membership.
S4 uses two production starts and distinct worker lanes to prove a three-member
resume remains correct under reverse-creation activation and ID-keyed roster
comparison. Accepted resumed worker prompts must publish live `active` stats and
leave the completed workers `active` for that daemon lifetime; an input-free
second cold resume must recompute their delegated `active_auto` defaults without
provider work. S5 correlates one held worker prompt across its durable dispatch
checkpoint, decoded fake cursor, and live readiness trace before process-group
`SIGKILL`. Two resumed boots require dispatch-uncertain warnings and zero
automatic worker provider turns; Boot B alone creates a fresh watch whose initial
typed status names the checkpointed prompt. This is a conservative recovery
oracle, not backend acknowledgement, exactly-once work, transactional checkpoint
coordination, or retry/abandon/recovery coverage.
S6 enables only the closed authenticated-release dummy hold for the worker and
deliberately never releases it, so elapsed time cannot terminalize the crash cut.
It kills after the durable request/start pair and canonical readiness, and observes
the durable `provider.tool_error` repair followed by its live `tool.error`
renderer projection, without redispatch. One explicit worker continuation
validates the exact balanced error round. A second resume consumes no input or provider action and must preserve
current/history membership, execution restore, and agent journals without a
second repair pair.
S7 combines a completed worker, a dispatch-uncertain worker, and an interrupted
dummy-tool worker below one quiescent main. Boot B and Boot C receive no input
between them and must spend zero provider turns, preserve exact lane ownership,
keep the uncertain worker undispatched, and add only Boot B's repair-worker
suffix. ID-keyed current/history rosters remain stable. A final explicit
repair-worker continuation after the second-resume assertions consumes only its
balanced error lane; it does not resolve the uncertain worker.
Embedded and
test-only daemon paths require no credentials, network, shell, sleeps, or VCR
gate. Panics, `run_turn` failures, and daemon exits before exact-consumption
acknowledgement retain the private generated configuration, scenario, durable
event log, extension stderr, and bounded semantic provider trace.

The Unix-only `core_resume` target additionally launches the exact universal
Tau binary under a fixed PTY for a fresh boot and explicit
`tau resume <session-id>` boot. Its VT model checks the completed dummy row after
Boot B historical restoration, then arms sticky monitoring to keep it terminal
through the fresh resumed turn. Boot A is allowed to show the ordinary live
pending state before completion.
Two live-attach variants compare the exact unique dummy-tool row across both
PTYs after completion and during a release-held invocation. The held variant
waits for the durable tool request and live correlated dummy readiness, sends one bounded
authenticated release frame, then compares both post-release `ok` and
`restart succeeded` snapshots. One typed terminal, durable CBOR snapshots, and the exact
two-action provider trace remain the lifecycle authorities.
A second gate creates and completes the production `agent_start` main/worker pair
through a headless Boot A with exact lane correlations, then starts only Boot B
under the public PTY. Stable IDs from typed protocol facts drive explicit
`:agent switch` commands for both restored transcripts and one targeted worker
follow-up. Replay boundaries, directed rosters, typed multi-agent store
prefixes/suffixes, exact provider consumption, and process-group/socket cleanup
remain independent oracles; the VT model proves only selection, terminal
historical rows, and transcript ordering.
For those two resume topologies, a side UI observer preserves replay metadata
and typed CBOR store reads prove identity and prefix/suffix durability.
A live-attach topology starts two PTYs against one daemon, attaches the
second exact public CLI by explicit session, and exercises both a completed text
turn and a correlated provider hold. The hold variant attaches only after the
prompt-specific hold-ready signal, requires both views to preserve the selected
agent, cancels once, and requires both views to settle on its editable status.
Typed stats prove the correlated running-to-idle transition. Normalized VT row
classes compare semantic elements rather than byte or cell identity.
A stable-ID variant completes the deterministic main/worker roster before
attachment, selects both agents in opposite orders from both UIs, and compares
ID-keyed transcript projections while causally checking each connection's local
selection. It covers only settled transcript materialization and selection
isolation; it does not extend concurrent prompt, tool, or arbitrary multi-client
ordering claims. The presentation variant also resizes one PTY from 120x40 to
72x24 and derives ordered worker prompt-boundary, response, idle, and selected-ID
classes while the
other remains on a distinct main transcript with a wide-only status signature.
It permits wrapping, spacing, adaptive field elision, and truncation positions
to differ; it does not claim arbitrary sizes or full-screen identity. See the
[`tau-e2e-tests` README](../crates/tau-e2e-tests/README.md) for the complete
coverage ceiling.
Replay-aware side observers, exact traces, provider stats,
bounded condition-driven waits, process-group cleanup, and runtime-artifact
cleanup remain independent oracles.
A third topology starts the universal PTY agentless and tool-free, then uses a
private exact same-process sender callback to authorize one bare external
message. That message auto-starts the first receiver. Typed socket stats prove
`active/running`; the correlated hold-ready notice is broadcast later, so
its target-PTY projection proves that UI consumed the update before one real
Ctrl-J selects the exact recipient. The test then cancels and reaps its correlated
provider hold. These are narrow terminal projection gates, not broad rendering
fidelity. They do not claim broader peer trust, delivery, provider, crash,
provider-builtin, upstream ChatGPT lowering/parsing, WebSocket, production
retry, or crash-exact replay coverage.

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

`ci.tests` is the mandatory selfci derivation. Its focused fake-provider and
provider-builtin post-checks pin exact same-profile subprocess paths and use
`--no-tests=fail` to prevent silent filtering; the Nix build sandbox denies
external network access independently of either fixture implementation.
Focused fake-provider unit tests own strict Configure grammar, resource bounds,
diagnostic truncation, malformed/mismatched checkpoint rejection, binding
uniqueness, cursor bounds, and barrier structure; the subprocess E2Es own representative lifecycle,
routing, streaming, mismatch, tool continuation, exact cancellation, bounded
timeout, concurrent isolation, clean restore, fatal disconnect, persistence,
and shutdown.

Provider fixtures submit explicit transient `provider.response_updated_reported` append
deltas and `provider.response_finished_reported` terminal payloads; consumer assertions
use the harness-canonical names. Raw-boundary fixtures assert the `_reported` names and
`persist=false` bit; wrong-owner reports still commit but derive no canonical fact.
Multi-update assistant/reasoning cases send only the
newly appended suffix. Do not feed full accumulated snapshots through delta helpers
unless the test explicitly checks legacy/invalid payload handling. Final-response tests
continue to assert complete canonical `provider.response_finished.output_items`.

Provider streaming tests must also assert response/progress rate-limit boundaries: non-terminal `provider.response_updated_reported` frames for one prompt publish the first non-empty streamed output sample promptly, suppress empty zero-byte idle samples until the first one-second deadline, batch later non-terminal samples to at most one per second, allow stats-only/no-byte samples as valid liveness updates once due, preserve suppressed visible deltas, and allow a terminal flush to publish the final batched suffix immediately before `provider.response_finished_reported`. Previous/current response-stat assertions should prove that `previous` equals the last emitted provider sample, not an internal suppressed calculation. Tests should also prove response bytes are recorded at the backend transport receive boundary before semantic parsing.

Harness tests for provider response stats should assert only validation and pass-through: wrong-owner reports commit observation-only, accepted stats-only reports derive canonical `provider.response_updated` events for subscribers, `agent_id` is rewritten from prompt ownership, and no separate harness-owned response-throughput projection is emitted.

First-output timing tests split ownership by layer. Backend parser tests assert
the exact accepted semantic predicate and dispatch boundary, including Codex
transparent-repair reuse; repetition-guard tests prove rejected deltas do not
mutate the accepted state inspected by that predicate. Provider sampler tests
assert capture before cadence filtering, immutable repetition, fresh-attempt
reset, and terminal flush. Protocol tests cover present, zero, absent/default,
and omitted wire forms. Harness tests only prove correlation and byte-for-byte
forwarding. CLI tests cover optional wording and compact-duration boundaries,
prompt/agent isolation, retry clearing, finish removal, and stale post-finish
suppression.

Provider retry scheduler tests should use injected/fake time and deterministic
jitter rather than real multi-minute sleeps. Cover retry-to-park handoff,
released worker capacity, due/fresh fairness, shared-cooldown extension,
prompt-scoped and global cancellation in queued/delayed/active states,
profile reload before a later success, and exactly one submitted/terminal
lifecycle. Backend fixtures should cover Codex WebSocket, generic Chat
Completions HTTP/SSE, and OpenRouter retry-then-success paths, including tentative
output clearing, trusted hint lower bounds for non-usage-window classes, and
bounded policy scheduling despite distant usage-window reset estimates.
Manual `:retry` coverage must drive the scheduler/runtime boundary
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
adapters: local Codex WebSocket failures plus generic Chat
Completions/OpenRouter HTTP/SSE failures must traverse production parsing and typed retry
classification. The joined incident gate may then consume that typed decision;
the feature-gated causal fixture carries its manually retried probe into an
embedded deterministic tool round, then feeds those exact committed events
through renderer state to assert main, global, and watched
activity become idle. These gates use loopback peers and explicit completion
signals only—never provider auth, Internet access, model prose, tmux, or sleeps.

Provider-builtin integration tests own profile serialization and CLI behavior,
model publication and routing, runtime event ordering, cancellation and retry
bookkeeping, and final provider event shapes. They use injected executors and
virtual monotonic time for scheduler retry and cooldown behavior, and temporary
auth files with injected endpoint outcomes for OAuth and credential-generation
behavior. Backend protocol matrices remain in the owning adapters.

### Exact provider-builtin subprocess acceptance

`provider_builtin_retry` is separate from `DeterministicFixture`: it runs the
exact Cargo-built `tau-ext-provider-builtin` binary through normal
Configure/Ready supervision, with a private keyless local Chat Completions
profile and a bounded joined `127.0.0.1` HTTP/SSE peer. The server scripts one
429 `rate_limit_exceeded` response with `Retry-After: 86400`, then P1 and
immediate P2 SSE successes. The test observes P1's typed `Throttle` retry
state, sends only the ordinary `UiRetryPrompt`, requires acceptance, and
combines wire capture, live lifecycle facts, and durable journal facts to
reject extra requests or upstream context drift, duplicate logical prompts or
terminals, and a stuck cooldown.

The same binary target also runs a closed Qwen3.8 text compatibility script
with the exact deterministic dummy-tool extension. It verifies literal `xhigh`
lowering, fixed template/sampling fields, streamed reasoning and visible text,
one tool call followed by two parallel calls, byte-exact raw argument replay,
tool continuation, and a usage-only terminal SSE chunk. It downloads no model
and contacts no external server.

Direct workspace runs intentionally skip this test when
`TAU_E2E_PROVIDER_BUILTIN_BIN` is absent; they must never discover a binary
from `PATH`. Run it manually only after building and pinning the candidate:

```sh
cargo build -p tau-ext-provider-builtin
TAU_E2E_PROVIDER_BUILTIN_BIN="$PWD/target/debug/tau-ext-provider-builtin" \
  cargo nextest run -p tau-e2e-tests --test provider_builtin_retry
```

The mandatory Nix post-check verifies that exact current-profile path and
exports the variable before running the test. It covers manual release of a
genuinely parked production retry, not automatic expiry, virtual clock
control, cancellation, restart, credentials, use of provider debug captures as
an oracle, or a retry-class matrix.

This exact subprocess case runs only on Linux, where it uses inotify for
non-polling daemon-socket readiness and Linux process-group teardown. Other
targets omit the case rather than weakening those lifecycle oracles.

The focused ChatGPT WebSocket lane stays inside `tau-provider-codex`.
Loopback-only finite peers exercise production request lowering and frame
parsing together with pool reuse, reconnect, cooperative cancellation,
provider-frame deadlines, typed errors, and the no-HTTP-fallback commitment.
Peers use synthetic credentials, explicit request/completion signals, bounded
scripts and socket deadlines, and joined teardown. Provider-builtin retry and
cooldown timing tests remain separate on its injected executor and virtual
monotonic clock; the exact subprocess acceptance above instead uses the ordinary
manual retry command and no clock control. Do not add a
harness→provider-builtin→local-WebSocket gate, backend
resolver, user OAuth URL override, or common scenario language merely to join
these layers; the deterministic fake provider does not cover upstream ChatGPT
transport contracts.

Provider-split acceptance is intentionally layered. Compatibility fixtures in
`tau-ext-provider-builtin` freeze existing serialized profile kinds plus the
generic Responses profile, durable old
session replay, model/routing publication, and successful event ordering. The
shared `tau-provider` crate owns summary-compaction materialization and limit
tests. The Chat Completions crate owns HTTP/SSE request, cancellation,
typed-error, Function tool, raw-argument, semantic-replay, transport-byte, and
summary wire-lowering tests; the extension owns OpenRouter discovery,
capability/default publication, public Responses fallback dispatch and
validation, sampling, events, and scheduler integration. The public Responses crate owns generic
`/responses` HTTP/SSE and WebSocket protocol coverage, transport selection
without fallback, full-replay retry semantics, no-`[DONE]` completion, typed
full replay, and Function-call raw-argument preservation. Its bounded loopback
WebSocket lane owns upgrade and HTTP-error handling, control-frame deadlines,
per-frame and cumulative response limits, and stalled-peer cancellation. The Codex crate owns
Standard/Lite goldens,
WS-only negotiation and no-fallback, typed finite outcomes, exact dispatch,
cumulative bytes, one-budget semantic-safe recovery, strict prewarm
prefix/fingerprint chaining, invalidation generations, compact cancellation, and
quota/OAuth parsing. `tau-provider` owns the currently documented shared outbound
route/proxy/TLS/redaction matrix.

Final compatibility checks should combine those focused package suites with the
network-denied curated VCR and deterministic fake-provider lanes, then run exact
`selfci check --candidate <commit>`. The fake-provider lane proves the generic
harness/extension lifecycle but does not replace either backend wire suite.
The shared outbound suite covers direct HTTP/HTTPS; HTTP and HTTPS through HTTP
and HTTPS proxies; WS through an HTTP proxy; and WSS through HTTP and HTTPS
proxies. It proves nested proxy TLS, CONNECT, target TLS, request/upgrade scope,
strict custom-CA parsing, immutable startup inputs, redaction, and no direct
fallback after proxy DNS, socket, TLS, CONNECT, target-TLS, or upgrade failure.
Reqwest's public API cannot expose a hidden CONNECT rejection status, so that
case remains redacted generic Proxy/Transport rather than exact authentication
classification.

## OAuth response safety

Codex OAuth protocol regressions live in `tau-provider-codex`: bounded response
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
`crates/tau-provider-codex/fixtures/provider-vcr/` is synthetic,
structurally sanitized evidence that representative Responses WebSocket frames
still traverse the production parser. It is not a
scheduler, retry-timing, concurrency, or model-output oracle; deterministic
runtime and local scripted-transport tests own those contracts.
Persisted transcript replay is likewise reconstruction evidence, not live
transport execution or a substitute for the focused localhost lane.
Its durable ownership and scope are recorded in
[`SPEC-tau-provider-codex-curated-vcr`](../crates/tau-provider-codex/specs/SPEC-tau-provider-codex-curated-vcr.md):
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

## Best-effort diagnostic workers

Keep serialization/redaction and failure-atomic append-primitive fault coverage
in the synchronous debug-log test writer. Exercise process-wide admission,
queued-plus-in-flight count/byte bounds, FIFO and path switching, per-line
sidecar reacquisition, recoverable omission/retry, uncertain-rollback poison,
and warning-episode accounting directly against the detached writer.
Use deterministic fault seams and channels/barriers for admission; do not infer
nonblocking behavior from sleeps or elapsed-time thresholds. Direct worker tests
must close their producer and join their test-only worker. Production
no-drain/no-join behavior is a structural review invariant: the singleton drops
the production join handle immediately, exposes no shutdown/drain API, and no
lifecycle code may wait for it.

Startup retention tests live beside `retention_cleanup`, `session_cleanup`,
`agent_cleanup`, and `diagnostic_cleanup`. Use injected clocks and filesystem
fault seams for exact inclusive age, scope, symlink, lock, detach/recreate,
tombstone, reference, and per-candidate failure behavior. The production worker
runs the phases once in that order and never blocks startup. Captures live under
`debug/provider-requests/<provider-instance>/`.

Provider-capture filename grammar tests live in
`tau-config::provider_debug_capture`, including `cache-diagnostic`. Scalar
admission/loss and in-flight budget tests live in
`tau-provider::cache_diagnostic`; Codex ordinary success, repair with a second
dispatch, replacement-upgrade failure, cancellation and immutable opt-out are
covered by deterministic loopback tests in `tau-provider-codex`.
Native compact coverage additionally pins admission rejection, final-outcome
validation/cancellation, compact-attempt ordinals, and absent exact-response
capability without retaining raw compact output. The closed-writer oracle
asserts identical attempted-enqueue correlation in exact and scalar request
captures for both inference and compaction.
Operation-capture protocol tests reject malformed IDs and raw-class pairing;
filename, opaque writer and cleanup oracles pin the separate private operation
grammar. Warm loopback tests cover backend success, repair, replacement-upgrade
failure, null prompt/ordinals, existing versus random operation IDs, opt-out and
pre-dispatch cancellation without extra provider traffic. The busy-pool oracle
retains reservation ownership and records a zero-dispatch backend summary.
The shared opaque writer and retention tests exercise the new class without
parsing it. Stage-1 inventory tests recognize it while preserving unavailable
analysis rather than treating file counts as attempts.

Writers and cleanup use that same dependency-neutral filename contract.
Worker and producer-integration tests live in
`tau-provider::debug_capture_writer`, `tau-harness::provider_capture_writer`,
and the concrete provider backends. Exercise immediate bounded admission,
Provider-side zstd round trips, redacted protocol Debug, late-session
attribution, exact opaque harness writes, and refusal of missing or symlinked
session/debug/capture directories. Compact HTTP failure tests must pin the
allowlisted headers, credential-only redaction, parsed-field bounds, 64-KiB
decoded prefix, complete/partial decoded hash coverage, filename grammar, and owner-only
directory/file modes. The process-wide sender deliberately
has no shutdown or join API: process exit can interrupt queued or in-flight
captures. A local-channel worker-drain test covers only the worker loop after
test producers disconnect and is not a production shutdown guarantee.


## Skill discovery and loading

`tau-skills` tests should cover frontmatter parsing, validation helper
contracts, deterministic directory discovery, bounded discovery reads,
symlink-following for roots/directories/Markdown skill files, canonical-directory
cycle prevention, collision winner selection, scoped prompt defaults, and built-in
self-knowledge skills. Prefer focused fixtures that exercise one contract at a
time, including oversized bodies/frontmatter and UTF-8-safe truncation edge
cases.


## Agent trace export

Agent-trace integration tests exercise durable journals through `AgentStore`.
Core coverage checks non-creating missing paths, lock-held checkpoint-bounded
prefixes, append/release before inactive lock acquisition, torn/corrupt/
non-monotonic rejection, sorted snapshot identities, and retained-lock
stability. Projection coverage checks lossless CBOR edge types, independently
parseable native lines, creator-only recursive descendants, unrelated invalid
creation artifacts, strict reachable-journal failure, descendant change during
snapshot preparation, per-agent order, focused typed lifecycle correlation,
decreasing-time handling, and protobuf-JSON decoding as one
`ExportTraceServiceRequest`.

CLI coverage verifies parser defaults and every format, custom agent roots,
descendant selection, and successful broken-pipe termination. Large-record and
many-record regressions
should assert that validation and raw projection stream records, payload-bearing
OTLP correlation data goes to anonymous staging, and heap correlation state
contains only compact offsets and identifiers. The accepted heap model is
proportional to unique typed operation ID count and bytes rather than journal
payload bytes; output is never truncated.
Compact semantic coverage additionally checks explicit observation identities,
selected-cut unresolved references, globally sorted absolute/relative journal
timing, provider item order, semantic prose/reasoning selection, both explicit
message directions, crash-tail incompleteness, source-owned output, bounded lite
content, complete full content, and faithful tagged-CBOR fallback. Structured
shell-outcome coverage enforces the exact raw-CBOR field/coherence matrix from
[`SPEC-durable-tool-observation-correlation`](../specs/SPEC-durable-tool-observation-correlation.md),
canonical foreground/background result and error ownership, cancellation/
placeholder/unresolved/non-shell omission, lifecycle-status independence, and
lite/full JSONL/TOON parity. Runtime tests
separately cover call-ID reuse and background completion correlation. TOON
coverage strictly decodes the counted item document, protects multiline/control
escaping and field-level Base64 framing, and compares its semantics with
independently parsed JSONL items.


## Semantic agent-status coverage

`tau-harness-tools` owns focused status argument validation. `tau-harness` owns
WorkStatus transitions, frozen status-capable tool-surface qualification,
foreground reminder settlement, persistent Working state, bounded post-append
Working and Unreported final challenges, status-unavailable bypass, delegated
completion/detach, append failure, and interception regressions. The
deterministic provider lane owns parallel accepted/rejected
Working calls, repeated work, Done/Blocked release, exact production watch
grammar, durable projection, and cold-replay oracles. Keep
parser-only tests as focused validation supplements; add production-boundary
regressions alongside each new handler or lifecycle integration.


## UI create-agent admission coverage

`tau-proto` owns wire round trips and transient classification. `tau-harness`
unit and socket tests own authorization, point-to-point admission, interception,
preprocessing, submission, cancellation, teardown, and no-replay behavior.
`tau-cli` owns request/agent/prompt filtering, admission timeout, unsuccessful
provider terminals, live-only interactive subscription, and output routing.
Deterministic process tests own end-to-end daemon/provider completion and
failure behavior.

## Start-agent atomic phase coverage

Treat acceptance, creation, membership, initial-prompt submission, and inference
dispatch as five independent publication outcomes. The focused harness startup
suite must assert canonical event sequence and cardinality, then use one shared
terminal helper to prove the operation map, request index, agent index, and
retained-byte total are all empty. Keep these deterministic cases:

1. every validation and runtime-bound rejection before acceptance;
2. acceptance pass, canonical replacement, drop, disconnect pass-through, and
   cancellation versus commit;
3. duplicate rebinding before acceptance, after acceptance, and after Agent
   installation, including collision-prone id templates;
4. failure before creation, before membership, before prompt, and before dispatch;
5. stream-local creation failure versus prompt commit, and owner-wide exit with
   durable and ephemeral starts in different phases;
6. canonical prompt replacement and drop, proving only committed replacement text
   reaches the selected provider;
7. every dispatch preflight rejection and a delayed checkpoint whose selected head
   covers the startup prompt through later committed facts;
8. cancellation and clean process/session shutdown at every phase boundary;
9. terminal interception/capacity retry with one failure and one directed result;
10. 64-operation, 4-MiB aggregate payload, and query-id limits with no rejected
    ephemeral-storage residue;
11. cold reopen after each durable prefix, proving incomplete starts are
    restored-unavailable and never replay-dispatched while a committed dispatch
    checkpoint retains ordinary recovery; and
12. post-membership failure followed by at most one unload, with runtime authority
    removed at failure commit even if unload publication parks or rejects.

The deterministic fake-provider restart scenarios own the cross-process completed
worker and incomplete-prefix cuts. Proto/client/CLI tests separately own the typed
failure payload, transient classification, accepted-placeholder projection, and
pre-membership phantom retirement. Do not replace these focused cuts with a broad
timing matrix or infer correctness from callback order.

The approved 18-row startup matrix is pinned by these exact named oracles:

1. `invalid_role_commits_before_directed_rejection`,
   `invalid_parent_commits_before_directed_rejection`,
   `loaded_parent_tool_owner_mismatch_commits_before_directed_rejection`, and
   the remaining validation cases in `interception::start_agent`;
2. `parked_acceptance_replace_and_drop_have_post_commit_visibility`,
   `preaccept_cancel_removes_parked_owner_without_failure_terminal`, and
   `acceptance_commit_racing_cancellation_emits_one_failure_obligation`;
3. `await_acceptance_duplicate_rebinds_without_early_projection`,
   `postaccept_preterminal_duplicate_rebinds_acceptance_and_failure`,
   `active_duplicate_rebinds_without_creating_another_agent`, and
   `start_coordinator_reserves_agent_ids_before_acceptance_commits`;
4. `dropped_agent_started_phase_commits_one_correlated_failure` and
   `wrong_family_agent_started_replacement_terminalizes_start`;
5. `accepted_start_storage_failure_terminalizes_and_continues_fifo`;
6. `stream_failure_races_startup_prompt_commit_without_double_terminal`;
7. `persistence_failures_target_exact_owner_generation_and_mixed_phases`;
8. `startup_prompt_replacement_reaches_provider_with_canonical_text_only`;
9. `post_membership_failure_removes_route_before_unload_resolves`;
10. `startup_interceptor_disconnect_passes_original_without_failure`;
11. `cancellation_terminalizes_every_startup_phase_exactly_once`,
    `acceptance_commit_racing_cancellation_emits_one_failure_obligation`, and
    `cancellation_races_startup_checkpoint_commit_with_one_winner`;
12. `committed_agent_started_runtime_install_failure_terminalizes_without_route`;
13. `settled_empty_model_inventory_terminalizes_accepted_startup`,
    `startup_provider_route_loss_rejects_before_checkpoint_and_delivery`, and
    `startup_checkpoint_semantic_admission_rejects_without_provider_delivery`;
14. `startup_failure_terminal_retries_once_after_capacity_wake`;
15. `session_shutdown_terminalizes_every_startup_phase_exactly_once` and
    `process_shutdown_terminalizes_parked_startup_prompt`;
16. `cold_restart_classifies_membership_and_prompt_prefixes_without_dispatch`
    `cold_restart_rejects_off_branch_checkpoint_as_startup_completion`, and
    `cold_restart_restores_checkpointed_start_without_coordinator`;
17. deterministic E2E `cold_resume_restores_completed_production_worker` and
    core-resume `public_terminal_cold_resume_selects_main_and_worker`; and
18. `post_membership_failure_removes_route_before_unload_resolves`, including
    warm pre/post-unload Current-roster assertions, plus
    `post_membership_unload_admission_rejection_restores_unavailable`.


## Self-compaction terminal crash coverage

Treat self-compaction acceptance, transaction outcome, background terminal,
typed prompt delivery, wait consumption, inference checkpoint, and provider
terminal as separate crash cuts. Deterministic harness tests must reopen before
and after each durable boundary, then reopen a second time to prove one typed
delivery, exact bounded correlation, no generic completion, consumed wait state,
and no replayed checkpoint. Cover success, started failure/cancellation,
pre-start failure, and explicit successor recovery independently; retain
cross-agent `agent_compact` as the asynchronous waitable control case.

## Automatic compaction lifecycle coverage

Treat the initial prompt, each post-tool prompt start, the final response or
canceled termination, outer-turn finish, and protected standalone start as
distinct ownership and crash boundaries. Core tests derive terminal ownership
from the exact `AgentPromptStarted.outer_turn_id`, cover initial and ordinary
post-tool prompts, reject missing or mismatched prompt ownership without
mutation, and compare live append with cold replay. Harness tests drive a real
tool round for normal and canceled continuation terminals and assert one durable
terminal, one matching finish, one protected start, no retained completion
after success, and idempotent restart repair. One deterministic fake-provider
scenario owns the cross-process post-tool lifecycle and compacted cold-replay
composition. Keep manual, before-inference threshold, reactive recovery,
output-length lineage, and provider request/response shapes in their focused
suites; do not replace them with a combinatorial matrix.

Restart repair is staged when a recovered finish creates protected standalone
work: the first reopen commits the missing finish/start suffix, the next records
the in-flight transaction as interrupted, and the following reopen is
quiescent. A prefix that already contains the standalone start owes the
interrupted outcome on its first reopen. Do not call the intermediate reopen
quiescent while protected provider work remains in flight.

## Reactive rolling compaction coverage

Treat the rejected inference terminal, initial reactive start, each partial
success, each predecessor-linked rolling start, the final activation-cut
arrival, and resumed inference as separate durable boundaries. Focused harness
tests must cover live and cold continuation below local projection, a small
retained activating input, and an activation cut whose logical provider window
begins at an earlier suffix-preserving replacement. Restart with removed route
or capability must commit one linked typed `route_failed` terminal instead of
checkpointing inference.

Keep replacement-plus-next-group no-progress separate: an unfitting rolling
prefix must commit one durable `prefix_too_large` terminal with no provider
prompt, no inference checkpoint, and the rejected activation retained. Core
live/cold folds own logical target projection and rejection of cuts beyond that
target; deterministic end-to-end coverage retains the existing opaque
standalone restart oracle rather than multiplying a broad matrix.

### Derived compaction-chain observability

`tau-core` reducer tests own explicit predecessor membership, correction
replacement, known/unknown/saturating cost folds, and missing or reversed
timestamp quality. Canonical `AgentTree` tests own valid-history live, cold, and
restart equivalence across decision, prompt-start, provider-response, accounting,
terminal, continuation, and branch-away/back cuts. Harness and deterministic E2E
tests continue to own recovery, dispatch, provider-attempt bounds, and generic
loop-policy behavior; the derived query has no policy consumer and does not
duplicate those suites.


## Output-length continuation coverage

Treat the source response, reserved steer, successor owner, prompt start,
successor terminal, and outer-turn finish as separate durable boundaries. Core
cold-replay tests must cover plan-without-steer, steer-without-owner,
owner-without-prompt-start, prompt-start-without-terminal, both values of the
terminal finish-repair bit, malformed or duplicate lineage, off-branch plans,
spent budgets, and multiple same-turn runs distinguished by source and successor
prompt ids. A qualifying selected-branch tool-call response must rearm live and
cold state at commit; length-truncated calls, empty calls, provider failures,
tool results, compaction, and off-branch calls must not. The owner and
prompt-start cuts must prove that restart does not resend.

Harness tests must cover adapter and originator eligibility, exact captured
model and route use, one continuation per consecutive reasoning-only run,
same-turn rearming after a committed foreground tool round, cancellation before
and after owner publication, branch loss, intercepted and append-rejected steer
or owner publication, terminal outcomes, tool-call suppression, and reactive
compaction. Client and worker tests must prove that incomplete output never
becomes a successful final, one-shot callers keep waiting across the planned
source, and provider watchers keep a sticky `output_length` terminal. Protocol
tests own JSON and CBOR round trips for the plan, owner, terminal, and watch
shapes.

Branch-movement coverage must exercise cold crash cuts after the dormant steer,
reserved owner, and synthetic terminal. Each cut keeps the sibling selected,
derives only the next exact repair, emits one visible notice after the owed
finish, and never creates or sends a provider prompt. Reselecting the repaired
branch must not revive a retired activation. Cancellation and append rejection
must still converge on that one dormant failure and leave later queued work
runnable.

Reactive-compaction coverage must reject and retry the planned rejection before
starting its transaction, then prove that only the exact transaction-owned
post-compaction descendant may close the output-length lineage. Cold replay must
preserve this ownership; an unrelated descendant must not inherit the spent
budget or terminal authority.

Provider-watch coverage must use a non-default durable provider attempt and
assert identical live and cold late-subscriber `terminal_incomplete` snapshots
in JSON and CBOR. It must also prove that a later selected success or unfinished
dispatch suppresses an older incomplete status rather than reviving it.

The deterministic fake-provider lane must issue exactly two provider requests
for an eligible source plus successor and must repeat the scenario after a cold
crash restart. Assert each response's usage and cost increment independently, then
assert their aggregate once. Keep these focused oracles when changing response
folding, recovery, accounting, adapters, or terminal presentation.


## Wall-clock timer coverage

`tau-ext-utils` keeps calendar parsing, DST gap/fold selection, and exact
large-gap counting in `daily_schedule` unit tests. Runtime tests inject host
timezone snapshots to cover the single refresh cadence, due-before-refresh
ordering, backward-clock progress, transient lookup recovery, and 60-second
runtime polling without changing the process timezone. Jiff owns platform
discovery and its approximately five-minute process-wide cache. Replay tests pair
recorded `tool.started` and successful terminal facts, then fold canonical timer
prompts and the agent replay boundary; retain the exact-minute start/result
crash regression when changing schedule reconstruction.

Keep parser/schema/display tests focused on the model interface. Put timezone
source, deadline, replay-fold, and scheduler-loop changes under the focused
runtime invariants above rather than relying on live host time or broad matrix
coverage.


## Provider cache-refresh lifecycle

Current deterministic coverage injects monotonic time and identity/jitter
entropy into scheduler tests for eligibility, economic evidence, bounded
eviction, window closure, cancel-without-release, source correlation, enqueue
failure, disconnect, and deadline equality. Harness tests cover canonical
terminal correlation and concurrent tool-cohort union. Built-in Provider runtime
tests cover directed successful completion; supervisor tests cover synchronous
directed cancellation and cooldown cancellation.

The deterministic fake-Provider E2E suite currently covers ordinary prompt/tool
turns with cache refresh disabled by default. It does not emulate cache-policy
evidence or an enabled refresh lifecycle. Add that fake lifecycle fixture before
depending on cross-process cache-refresh E2E coverage; never use live Provider
cache behavior as the deterministic oracle.

Repeated-wait guard coverage uses focused harness tests with supplied monotonic
clocks rather than deterministic provider E2E sleeps. Those tests exercise
ordinary timeout publication, one-shot/reset/suppression policy, exact and bare
background-wait exclusion, and manual-compaction rollback settlement. Add
deterministic E2E coverage if wait bounds become cheaply configurable in that
suite or if provider-to-handler call correlation changes.
