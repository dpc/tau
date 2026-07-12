# Design decisions

This file records major design decisions currently embodied by this directory's
code, and how authoritative each decision is. It is not an architecture overview,
ADR log, todo list, roadmap, implementation guide, or changelog.

## Provider response stats are provider-owned public events

Status: confirmed, 2026-07-08, user

Providers own prompt-local response byte counting and rate limiting because they dispatch backend requests and read upstream response bytes at the transport boundary. They may attach `response_stats` previous/current samples to `provider.response_updated`, including stats-only updates with no text deltas. The first non-empty sample may be emitted promptly; later non-terminal samples are emitted at most once per second per prompt, with an optional terminal flush before the provider prompt closes.

The harness must not account, sample, remap, strip, or project provider response throughput. Its role for `provider.response_updated.response_stats` is only the normal provider-event boundary: validate provider prompt ownership/cancellation, rewrite routing identity from prompt ownership, enrich unrelated compaction metadata when applicable, and broadcast the provider-owned sample unchanged to subscribers. UI clients render live response throughput directly from provider events.

## Durable compaction and activation binding

Status: confirmed, 2026-07-11, user

New inference checkpoints own a complete provider-qualified model, inference
operation, and activation-cut tuple together with their prompt, transcript
watermark, and optional compaction transaction. Post-commit materialization,
parameters, tools, accounting, and point-to-point provider routing use that
ownership rather than mutable model selection. A transaction checkpoint
must match its standalone start's model and cut; if the exact route disappears
before commit-time delivery, providers are excluded and the owner is durably
terminalized without remote send.

Standalone compaction binds durable Started and Compacted facts with the exact
transaction, cut, suffix end, pre-minted prompt id, provider-qualified model,
and standalone operation. New boundaries require all six fields: the
transaction resolves its Started fact; cut, prompt id, model, and operation
match Started; operation is standalone; `suffix_end` equals the boundary
parent; and cut is its ancestor. Legacy boundaries have all six absent. The
provider connection id is runtime-only and must not be persisted.

Canonical submitted, injected, and steered facts contain a harness-owned,
default-false `inference_activation` marker. Typed harness provenance marks
passive background/restore context false and actual activators true; neither
prompt text, peers, nor interceptors control it. Completed checkpoints consume
true activations through their branch head. A checkpoint without a durable
terminal response restores as dispatch-uncertain and is not resent.

The cross-crate test strategy fixes these boundaries at their owning layer:
`tau-proto` covers missing/false/true serde behavior; `tau-core` covers the
all-six group, exact mismatches, duplicate/unknown outcomes, and legacy
boundaries; and `tau-harness` covers Started-before-dispatch and terminal
correlation, interception/peer ownership, typed passive replay, crash restart,
checkpoint ranges, and dispatch uncertainty.
Restored post-compaction continuation coverage includes captured-route success,
staggered unrelated discovery, discovery-complete absence, explicit model
removal, warm resume, mutable role/model drift, sanitized terminal visibility,
and replay exactly-once behavior.
Pre-Ready provider model updates are coalesced per provider before activation.
The final staged snapshot determines captured-model presence; earlier staged
presence followed by final omission is an authoritative removal, while absence
throughout remains unresolved until discovery completes. Awaiting-checkpoint
runtime state carries provider-qualified model, inference operation, and
activation cut as one complete ownership value.

## Daemon listener shutdown is reactive and path-independent

Status: unconfirmed

The harness daemon accept forwarder must not poll on fixed sleeps while waiting
for clients or shutdown. It should block in an OS readiness primitive over the
listener fd and an owned wake/cancellation fd, then accept ready clients and exit
promptly when the wake fd fires.

Shutdown must not rely on reconnecting to the daemon socket pathname. Runtime
socket paths can be removed or replaced while the listener fd remains alive, so
the wake primitive has to be owned by the forwarder thread. Tests for this area
should cover idle shutdown, missing/replaced socket paths, and the invariant that
internal wake traffic is never delivered as a harness client.

## Harness lifecycle tests cover state and replay contracts

Status: unconfirmed

Harness lifecycle/startup changes should prefer focused unit or lifecycle tests
that exercise the state machine directly, then rely on broader crate tests and
`selfci` for regression coverage. Tests for startup, disconnect, and optional
extension behavior should assert both the immediate state transition and the
replay/delivery contract for mandatory diagnostics: initial publication is not
enough if late UI subscribers must understand what happened during startup.

For optional-extension startup work, cover required/default compatibility and
each optional failure path being changed, such as config/secret/spawn failure,
pre-Ready disconnect or timeout, and `ConfigError` handling. Avoid slow wall-clock
timeout tests when a private helper can drive the same branch deterministically.

## Harness-owned tool prompt-surface policy uses prompt snapshots

Status: unconfirmed

Extensions and providers publish neutral metadata (`ToolTag` and `ModelTag`) but
must not decide which model receives which tool surface. The harness evaluates
configured `tool_policy.rules` and role overrides when building the provider
prompt's effective tool list. Built-in model-specific behavior, such as the
ChatGPT shell surface, must be represented as ordinary policy data so user config
can disable or replace it by keyed rule name.

Provider tool-call authorization is against the tool snapshot advertised with the
owning prompt. Mid-turn role/model switches or later staged tool registrations
may affect future prompts, but they must not expand or shrink the set of tools
accepted for an already-dispatched prompt.

Model-visible rejection diagnostics for prompt-owned calls follow the same
authority boundary. If a rejected call is tied to an `AgentPromptId`, unavailable
or near-name diagnostic text must derive from that prompt's tool snapshot rather
than the current role/model surface.

Tool examples attached to registrations are deliberately excluded from rendered
provider tool definitions. The harness may append one bounded example to a
model-visible failure for the owning agent branch, then remembers that example so
retry loops do not receive repeated scaffold text.

Harness tests should assert both sides of that contract: examples are omitted from
rendered provider tool definitions for good calls, and failure-triggered injection
is one-shot per agent branch while invalid registrations produce mandatory
diagnostics.

## Cross-harness agent messages use a dedicated asynchronous RPC

Status: unconfirmed

The harness-owned `message` tool treats `<session-id>/<agent_id>` as an external
address only when the session id differs from the current active session. It
must not pack the session id into `AgentId`; protocol and event payloads carry
session and agent identity separately.

External delivery uses a dedicated socket RPC, not generic `emit`, and the
runtime-dir lookup plus socket round-trip runs off the event loop. The helper
thread reports completion back with a harness command. Sender-side
`agent.message_sent` projections represent confirmed delivery, so lookup/socket
or target validation failure completes the tool with an error without recording a
successful send projection.

Runtime-dir stale cleanup is conservative: failed socket probes must not remove
discovery files while the advertised daemon pid is still live. Dead-pid entries
remain eligible for cleanup where Tau has a safe pid-liveness backend, but a
transient connection failure must not make a running daemon permanently
undiscoverable to external-message lookup or CLI attach.

Receiver-side sender authentication must not block the central harness event
loop. After cheap target validation, callback socket discovery and I/O run on a
helper thread and return a harness command that sends the RPC result and commits
the inbound projection only after the claimed sender authorizes the exact sender,
recipient, kind, and message body fields.

Tests should cover the runtime metadata active-session contract, stale/ambiguous
discovery, untrusted peer rejection, target-session and recipient validation,
external prompt/UI labels, sender capability binding, non-blocking receiver-side
authentication, and failure not publishing a sent projection.

Schema-guided argument repair runs only in the pre-dispatch validation failure
branch. The harness executes a repaired call only after the repaired arguments
pass the same schema validator, emits a non-mandatory notice/log trace for the
local repair, and otherwise preserves the rejection/error/example behavior used
for unrepaired failures.

Testing is split by owner. `tau-config` tests cover file and CLI alias
normalization, keyed rule layering, and tag-pattern parsing/rejection.
`tau-harness` tests cover evaluator ordering, role broad-to-specific overrides,
the built-in policy through the shared evaluator, and prompt-owned snapshot
authorization.

## Runtime loop guard

Status: unconfirmed

The runtime loop guard is intentionally conservative and per-agent/branch only.
It tracks a bounded recent signature window and bounded breaker bookkeeping,
injects at most one internal pivot prompt for an obvious repeated cycle, and then
stops automatic continuation with a mandatory notice if the same cycle persists
after the breaker was dispatched.

Provider `repetition_detected` responses are treated as a loop-guard trigger with
a fixed harness-authored reason. The provider error is display-only; it is not
used as trusted pivot text.

New non-internal user input resets the guard even when the prompt is queued, and
successful foreground or background tool results reset it as clear progress.
Progress resets clear detector/breaker history and stale queued pivots but keep
unresolved in-flight tool-call argument signatures, so a successful sibling tool
in a multi-tool turn cannot make later failures argument-insensitive. Non-linear
branch/head moves invalidate the whole branch-local guard, including in-flight
signatures, and remove queued loop-guard pivots. Tests should exercise the
production response/tool/prompt wiring, not only private detection helpers: text
loops, repeated identical failures, different failure streaks, A/B/A/B suffixes,
queued user-input reset, success reset, branch invalidation, bounded breaker
state, argument-sensitive tool failures, and same-batch tool failures that must
receive the breaker before blocking.

## Ephemeral sessions suppress only session-owned persistence

Status: unconfirmed

`tau --ephemeral` is a harness/session launch mode, not an agent privacy mode.
It keeps the live session state machine, interception, prompt dispatch, and
agent stores working normally, but session-owned persistence is runtime-only for
that harness process: session membership logs, session metadata/locks,
`events.jsonl`, per-session stderr logs, and session-scoped extension data are
not written. `harness.session_dir` uses status `ephemeral` and a display-only
`<ephemeral>` path so UIs do not advertise a usable session directory.

Agent transcripts remain durable by default, including sub-agents started by
`agent_start`. Per-agent ephemerality is a separate creation policy staged from
the TUI with `/new` then `/ephemeral on`; it keeps that agent's semantic
transcript, metadata, and session membership in memory until daemon exit.
Children of ephemeral parents inherit ephemerality. Provider state, credentials,
policy/config files, runtime sockets, user/cache extension data, durable
recipients/parents, and tool side effects keep their normal persistence.

## System prompts are assembled only through templates

Status: confirmed, 2026-06-17, dpc

System prompts must be assembled through the prompt templating system. Any new
dynamic system-prompt value must be an explicit template variable/input, not text
formatted, prepended, appended, replaced, or otherwise edited around rendered
prompt content.

Ad-hoc string surgery for prompt variables such as `agent_id` is forbidden both
before and after template rendering. Exceptions are only for clearly documented
transport concerns that are not system-prompt content. This keeps custom
templates in control of placement and wording for dynamic values.

## Agent watch outer agent-turn lifecycle

Status: confirmed, 2026-07-10, dpc

`agent_watch` observes the canonical two-state outer agent turn: idle versus
running from activating input through the terminal response or termination.
Inner model rounds and intervening tool rounds remain in the same agent turn.
A new watch receives one
initial snapshot; genuine transitions are receiver-only durable notifications
with subscription identity and watched-agent runtime generation. Content
forwarding remains limited to direct user prompts and final responses.
Lifecycle-notification-only turns suppress both state edges to prevent cyclic
watch amplification. If ordinary input joins such a running generation, a
delayed start is emitted before the eventual matching stop.

Enable lifecycle classification and watch mutation form one authoritative
harness-loop operation. Only a Live target can create topology, subscription,
or notification state; Stopped and Unknown failures change none of that state.
A same-id reload remains unwatched until an explicit enable creates a fresh
subscription, while disable stays idempotent for known stopped endpoints.

The initial snapshot remains a durable client-visible fact but is not queued or
replayed into the watching model's context. Live delivery and transcript replay
derive later model-visible transition wording from the structured watched-turn
payload and watched-agent identity. The durable compatibility `message` text is
not authoritative presentation or model input.
## Prompt capability truth

Status: unconfirmed

Prompt templates receive sparse capability data owned by the harness. For each
turn the harness resolves the concrete agent role/model and one effective tool
snapshot after policy and provider-supported-type filtering. That snapshot is
the source for tool definitions, authorization, tool fragments, and
`capabilities.tools.available`; non-tool extension side queries intentionally
receive an empty available list because local authorization forbids calls even
though wire definitions remain cache-compatible. Extension enabled/Ready state
is captured at render time. Render failures are explicit and prevent provider
dispatch; capability state is not persisted separately.

## Agent unload retires watch endpoint state

Status: unconfirmed

A committed unload of either endpoint retires every incoming and outgoing
watch relation, subscription identity, current provider snapshot, and
per-subscription delivery state. The local removal fallback is idempotent with
the committed reaction. Later fanout requires both endpoints to remain live,
surviving watchers receive an authoritative replacement snapshot, and loading
the same agent again requires a fresh subscription. Harness durable-log tests
cover both endpoint directions, replacement topology, absence of post-unload
retry/recovery/terminal fanout, and fresh state after same-session reload.

## Watcher-visible provider work

Status: confirmed, 2026-07-11, dpc

Provider retries carry closed structured categories, saturating attempt counts, and approximate bounded delays independently of human UI prose. After validating prompt ownership, the harness owns the current per-agent/turn/prompt snapshot and session-local watcher fanout. Live delivery is limited to first category, category/phase changes, and terminal failure; same-category storms only refresh the late-watch snapshot. Enabling or re-enabling returns current sanitized state and emits an initial client snapshot without prompting the model. Durable live facts replay as transcript context without re-fanout; disable, prune, and session change stop delivery. Raw provider bodies, status text, errors, headers, account data, secrets, and prompt content never cross this boundary.

## Reactive overflow transaction

Status: confirmed, 2026-07-11, dpc

Eligible ordinary inference overflow is durably recorded before one correlated standalone compaction starts. The compact transaction claims the failed prompt, compacts only through the pre-activation cut, and resumes through the original checkpoint so concurrent suffix facts and the owed activation are preserved. Any second overflow or ambiguous dispatch is terminal rather than recursive.

Testing is split by owner: `tau-proto` fixes default and tagged wire forms; `tau-core` fixes unique claim validation and planned/claimed replay folding; `tau-harness` covers eligibility, durable ordering, no-recursion, continuation, watcher projection, and crash cuts.
## Model-callable manual compaction

Status: unconfirmed

`compact` and `agent_compact` are independent disabled-by-default internal
tools. Prompt-snapshot presence is the capability: `compact` targets only its
owner, and `agent_compact` targets any other loaded same-session agent without
an ancestry test. The harness records a bounded request id, caller, target,
prompt, tool call, model, and accepted head before returning its background
placeholder. Replay either waits for a complete self tool round, starts the
transaction once, or reconstructs its missing terminal background completion.

Testing ownership is split by boundary: `tau-harness-tools` owns schemas,
strict parsers, and independent groups; `tau-core` owns request/start/failure
validation plus durable and memory-only replay; `tau-harness` owns prompt
snapshot authority, loaded-target and state matrices, complete sibling-round
ordering, arbitration, provider terminals, cancellation, crash repair,
exactly-once background completion, watcher sanitization, and continuation
checkpoints.
