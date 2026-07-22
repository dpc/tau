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

Inside the isolated session, create a main and worker, inspect `/agent`, select
each with `/agent switch <agent-id>`, and send one follow-up. From a separate
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
pass `-r` to its child Tau, and its tmux session ends when Tau exits. It therefore
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


## Provider response streaming tests

### Deterministic full-harness provider acceptance

`tau-e2e-tests` has an always-on `DeterministicFixture` that launches the
test-only `tau-e2e-fake-provider` by exact path through normal extension
supervision. Its strict inline `ScenarioV1` drives synthetic streaming and a
real deterministic `tau-ext-test-dummy` tool continuation. `ScenarioV2` adds
bounded exact-correlation lanes for typed failures, cancellation/timeout and
same-agent post-cancel liveness,
barriers, fatal disconnect, quiescent same-agent restore, and one closed
`restart_test_dummy` call/result pair. Its session-restore grammar also has one
exact production `agent_start` pair, a harness-minted child binding, and bounded
typed automatic-watch matching. The corresponding two-agent gate proves a
completed durable worker cold-restores with its own route and transcript while
the daemon-lifetime watch is absent. Its S2 grammar adds one closed
`AgentWatchCall`/`AgentWatchResult` pair, and the fresh fixture proves a new
subscription produces exactly one non-model initial snapshot plus one
prompt/running/response/idle set under only the two contract causal edges.
S3 reuses the S1 grammar, creates one promptless ephemeral worker through the UI,
and seeds a typed durable worker load/unload history between clean boots. Its
current/history roster, route-rejection, replay, typed-store, and exact-lane
oracles prove Boot B creates routes only for the current durable pair, preserves
the unloaded worker only in history, and drops ephemeral membership.
S4 uses two production starts and distinct worker lanes to prove a three-member
resume remains correct under reverse-creation activation and ID-keyed roster
comparison. S5 correlates one held worker prompt across its durable dispatch
checkpoint, decoded fake cursor, and live readiness trace before process-group
`SIGKILL`. Two resumed boots require dispatch-uncertain warnings and zero
automatic worker provider turns; Boot B alone creates a fresh watch whose initial
typed status names the checkpointed prompt. This is a conservative recovery
oracle, not backend acknowledgement, exactly-once work, transactional checkpoint
coordination, or retry/abandon/recovery coverage.
S6 enables only the closed `hold_no_side_effect` dummy tool for the worker,
kills after its durable request/start pair and canonical readiness, and observes
the eager live `tool.error` then durable `provider.tool_error` repair without
redispatch. One explicit worker continuation validates the exact balanced error
round. A second resume consumes no input or provider action and must preserve
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
`tau -r <session-id>` boot. Its VT model checks the completed dummy row is always
terminal throughout Boot B historical restoration and the fresh resumed turn;
Boot A is allowed to show the ordinary live pending state before completion.
A second gate creates and completes the production `agent_start` main/worker pair
through a headless Boot A with exact lane correlations, then starts only Boot B
under the public PTY. Stable IDs from typed protocol facts drive explicit
`/agent switch` commands for both restored transcripts and one targeted worker
follow-up. Replay boundaries, directed rosters, typed multi-agent store
prefixes/suffixes, exact provider consumption, and process-group/socket cleanup
remain independent oracles; the VT model proves only selection, terminal
historical rows, and transcript ordering.
For those two resume topologies, a side UI observer preserves replay metadata
and typed CBOR store reads prove identity and prefix/suffix durability.
A third topology starts the universal PTY agentless and tool-free, then uses a
private exact same-process sender callback to authorize one bare external
message. That message auto-starts the first receiver. Typed socket stats prove
`active_auto/running`; the correlated hold-ready notice is broadcast later, so
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

`ci.deterministicE2eTests` is a mandatory selfci derivation. Its exact target
plus `--no-tests=fail` prevents silent filtering, and the Nix build sandbox
denies network access independently of the fixture implementation.
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

Provider retry scheduler tests should use injected/fake time and deterministic
jitter rather than real multi-minute sleeps. Cover retry-to-park handoff,
released worker capacity, due/fresh fairness, shared-cooldown extension,
prompt-scoped and global cancellation in queued/delayed/active states,
profile reload before a later success, and exactly one submitted/terminal
lifecycle. Backend fixtures should cover Codex WebSocket, generic Chat
Completions HTTP/SSE, and OpenRouter retry-then-success paths, including tentative
output clearing, trusted hint lower bounds for non-usage-window classes, and
bounded policy scheduling despite distant usage-window reset estimates.
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

The focused ChatGPT WebSocket lane stays inside `tau-provider-codex`.
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

Provider-split acceptance is intentionally layered. Compatibility fixtures in
`tau-ext-provider-builtin` freeze all three serialized profile kinds, durable old
session replay, model/routing publication, and successful event ordering. The
Chat Completions crate owns HTTP/SSE request, cancellation, typed-error, Function
tool, raw-argument, semantic-replay, and transport-byte tests; the extension owns
OpenRouter discovery, capability/default/explicit-false publication, sampling,
events, and scheduler integration. The Codex crate owns Standard/Lite goldens,
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


## Skill discovery and loading

`tau-skills` tests should cover frontmatter parsing, validation helper
contracts, deterministic directory discovery, bounded discovery reads,
symlink-following for roots/directories/Markdown skill files, canonical-directory
cycle prevention, collision winner selection, scoped prompt defaults, and built-in
self-knowledge skills. Prefer focused fixtures that exercise one contract at a
time, including oversized bodies/frontmatter and UTF-8-safe truncation edge
cases.
