# SPEC-tau-e2e-deterministic-provider: Deterministic provider acceptance contract

## Record justification

This cross-cutting contract coordinates generated role and extension
configuration, the fake provider's scenario grammar and durable checkpoint,
daemon-side production-tool injection, typed multi-agent store snapshots,
replay-aware socket observation, and the exact deterministic CI lane. Keeping
the acceptance boundary here prevents those independently maintained pieces
from silently broadening fixture authority or weakening the claimed oracle.

Deterministic harness acceptance uses a test-only supervised provider subprocess
inside `tau-e2e-tests`. It is launched by exact path as a required custom
provider and is never added to the universal binary, built-in registry, normal
provider discovery, or provider self-knowledge.

Control is strict versioned `ScenarioV1` or `ScenarioV2` data supplied in startup
Configure. V1 uses one global FIFO; V2 uses independent lane-local FIFOs. Actions
match selected stable typed prompt projections and emit explicit transient provider
execution reports. Dynamic prompt identities are copied from the request; provider-authored
tool call IDs must return unchanged in tool-result continuations. Unknown
configuration, unexpected prompts, overlaps, first mismatches, and unconsumed
actions fail closed with bounded synthetic diagnostics.

Scenario `user_text` remains semantic accepted text. Closed fixtures reserve the
exact canonical fieldless `<user>...</user>` syntax for HumanUi provider
projections; the fake cannot infer durable provenance from provider text alone.
It decodes that reserved syntax before V2 lane and action matching, while every
other fixture string remains literal. A future non-Human fixture that needs the
same literal syntax requires a typed scenario distinction rather than heuristic
inference. V1's first text and tool-call actions additionally require the exact
HumanUi envelope, so the always-on deterministic lane fails if interactive
prompt projection disappears or changes structure.

The fixture uses fresh private config, state, session, and artifact directories.
It disables every unrelated built-in extension and normally enables only the
no-side-effect `tau-ext-test-dummy` success mode. Gate 2 is one controlled
exception: the exact universal `component ext-shell` may expose only `workdir`
and `edit` to a closed scratch-only scenario. S1 is the other: its main role
exposes only the production harness-owned `agent_start` built-in while its
worker role exposes no tools. S2 adds only production `agent_watch` to that
main role. S3 reuses S1's exact roles and grammar; its promptless ephemeral agent
and typed unloaded-worker store records consume no fake-provider action. S4
instead configures two distinct tool-free worker roles and keeps only
`agent_start` on the main. S5 reuses S2's two-role tool surface for one
synchronized interrupted-worker restore. S6 instead exposes only
`restart_test_dummy` in exact `hold_no_side_effect` mode to the worker while the
main retains only `agent_start`. S7 uses one main, two tool-free worker roles,
and one repair-worker role exposing that same sole dummy. Its main consumes the
existing two production-start pairs; one fixed durable UI creation supplies the
repair worker without extending the fake grammar. S8 reuses S1's fixed
production-main and tool-free worker roles. Its headless Boot A enables only the
fake-provider extension and exposes only harness-owned `agent_start` to the main;
its universal PTY Boot B preserves that exact extension/tool surface. The fake
has no network,
authentication, shell, evaluation, child-spawn, prompt-control, environment
control, or arbitrary fixture-file behavior.
The separate peer-navigation PTY case starts with no agent, exposes no tools,
and authorizes one exact external message through a fixture-owned same-process
callback endpoint. Its sole provider action is a bounded hold used to inspect
the live navigation interval.

Hermetic embedded and daemon launches bypass ambient Tau startup-role,
role/config, extension, and secret environment transports, retain that policy
for runtime settings reloads, then check an exact extension-name allowlist before
any process starts. This is a deterministic-test exception to
normal interactive startup availability in
[SPEC-tau-harness-extension-lifecycle](../../tau-harness/specs/SPEC-tau-harness-extension-lifecycle.md);
embedded launches do not scrub the ordinary child OS environment. Spawned daemon
acceptance clears it and supplies private HOME/XDG roots plus a fixed locale.

`ScenarioV1` remains the closed phase-one grammar. `ScenarioV2` adds at most
eight exact `ctx_id` lanes with independent bounded cursors. It supports typed
terminal errors, exact cancellation holds with hard timeouts, deliberate
disconnect, and named barriers whose participants must all submit before any
lane completes. It also has one narrow adjacent action pair for the allowlisted
`restart_test_dummy` empty-argument call and exact successful result; arbitrary
tool names, arguments, and results remain outside the grammar. S6's separate
closed repair sequence accepts the same sole dummy call only when followed by
the exact synthetic interrupted error status and diagnostic, terminalizes that
next explicit worker continuation only while the balanced call/error pair
remains in context. S1 adds one
distinct adjacent `AgentStartCall`/`AgentStartResult` pair; S4 raises that
closed bound to two unique adjacent pairs. The fake requires
exact bounded instruction, optional role, task name, call id, production tool
name/type/schema, one sole successful result in the latest continuation block
with an exact two-field payload,
and harness-minted distinct
self/child identities. It retains the ordered parent/child associations and
matches automatic watch traffic against the latest successful start. S2 adds
one adjacent `AgentWatchCall`/`AgentWatchResult` pair and therefore still
requires exactly one retained child.
It derives the exact watched ID only from that retained association, permits
only `enable: true`, requires the production name/type/schema, and accepts one
correlated successful result whose exact sanitized text names the child and
contains no subscription identity. No other harness-owned tool enters the
grammar.

S1 also adds one bounded `WatchNotifications` action containing one to four
typed `Response`, `Prompt`, or non-initial `TurnState` records. Each provider
prompt consumes and validates the complete already-delivered queue prefix for
the current closed action; an incomplete prefix returns fixed text without
advancing the lane action. The action requires the retained child identity,
exact sender/recipient, kind, content or runtime state, and one stable
subscription-id/turn-generation pair across its turn-state records. It also
checks the exact ordered, escaped model-visible prompt projection. Only records
for the current closed action enter the queue; unrelated, reordered, and excess
live traffic fails before admission. Replayed deliveries cannot populate this
live queue.

S2's closed `WatchNotificationChains` action requires the explicit watch to
create exactly one non-model initial idle
snapshot with a new nonempty subscription ID distinct from Boot A. Its one
fresh direct worker turn then yields exactly one prompt notification, running
edge, final-response notification, and idle edge. Only prompt-before-response
and running-before-idle are ordered across those streams. The later turn-state
edges retain the new subscription and generation; the initial snapshot consumes
no provider action.

A barrier is the lane's sole action, appears once per distinct
participant lane, and has one consistent bounded participant count, preventing
same-lane and cyclic barrier plans. Initial `ctx_id` binds an agent to one lane.
The public terminal UI supplies no initial `ctx_id`, so an unbound first prompt
may select the sole configured lane. S1's production-started worker also has no
initial `ctx_id`; neither do S4's two production-started workers. Their exact
first user text must select exactly one unconsumed, unbound lane. Zero or
multiple candidates fail closed. Every selected binding is immutable and
checkpointed. Other multi-lane agents still require an exact `ctx_id`.
Continuations cannot change that binding. The fake subscribes
only to live prompt/cancel/watch traffic, so restored event replay cannot
consume actions.

The core-shell action family is likewise closed rather than generic: it can set
only relative `project`, edit only `resume-sentinel.txt` with the two fixed
line-range shapes, and vary only bounded call IDs, nonce text, prompts, and final
markers. Its result continuations require success; the resumed provider prompt
must contain both the old nonce-bearing transcript and restored workdir context.

V2 cursors, immutable agent-to-lane bindings, and bounded harness-minted
parent-to-child associations are atomically checkpointed in the harness-assigned
extension state directory and restored only after validating the complete
scenario identity, a 64 KiB checkpoint ceiling, binding uniqueness,
parent/start-result correlation, contiguous per-parent start ordinals, and
cardinality bounds. This supports
clean, quiescent daemon stop/resume with no in-flight action. The checkpoint and
harness journal are not transactionally committed together, so this fixture
makes no crash-exact action-replay claim; such a claim requires a provider
acknowledgement protocol design.

S5 deliberately uses one existing `HoldUntilCancel` action without issuing a
cancellation. After the fake commits the worker lane cursor and starts the
bounded hold worker, it appends one exact prompt-correlated `hold_ready` semantic
trace record and emits the same correlation as an info-level notice. The test
separately observes the same prompt in the durable
harness `agent.inference_dispatch_started` journal and decodes the fake
checkpoint's exact scenario, worker lane binding, and next-action cursor before
sending `SIGKILL` to the private daemon process group. The readiness record is
only synchronized live fixture observation: it is not a durable backend
acknowledgement and does not make the two independent stores transactional.

On each of two cold resumes, S5 requires both durable routes, the mandatory
dispatch-uncertain warning, and zero automatic provider submissions. Boot B
recreates a fresh main-to-worker watch through the closed S2 action pair and
requires its initial structured provider snapshot to carry the original prompt
id and `dispatch_uncertain/unknown`; the old automatic watch remains absent and
the initial snapshot consumes no provider action. A closed result-expectation
variant makes the fake require the corresponding exact sanitized tool result,
while S2 still requires its status-free result. Boot C supplies no agent input and must
reproduce the warning and zero-provider fail-closed state. S5 does not retry,
abandon, cancel, or otherwise recover the uncertain work.

S6 uses the closed hold boundary from
[ARCH-tau-ext-test-dummy](../../tau-ext-test-dummy/specs/ARCH-tau-ext-test-dummy.md).
Boot A requires exactly one correlated worker `tool.request` followed by canonical
`tool.started` in the typed execution-restore stream and one live readiness fact
before the process-group kill. Boot B requires one live nonsemantic `tool.error`
then one durable `provider.tool_error` with the full restart/possible-side-effect
diagnostic, no live dummy redispatch, and one explicit worker continuation whose
complete tool-result context is exactly that balanced error. Boot C receives no
input and must add no repair; its current/history membership, execution restore,
current-agent journals, and separately loaded worker journal equal Boot B.

S7 uses four lanes and compact 2,445-byte scenario JSON: six main actions, one
completed-worker action, one uncertain hold, and the two-action dummy repair
pair. Boot A consumes eight main turns and one turn per worker, checkpointing
exact cursors `[6, 1, 1, 1]`, four immutable lane bindings, and two contiguous
production child ordinals before the combined crash cut. Boot B and the
no-input portion of Boot C consume no provider action. Only the uncertain worker
may own the exact per-generation warning, only the repair worker journal may
gain Boot B's provider error, and Boot C's durable snapshot must equal Boot B.
One explicit repair-worker continuation after that equality check advances only
its cursor to `[6, 1, 1, 2]`; the uncertain dispatch remains unfinished.

Daemon acceptance uses the normal local socket protocol and real supervised
subprocess. Its `ServeOptions` explicitly bypass ambient startup override
transports and checks the same exact extension allowlist before spawning as the
embedded fixture. Normal daemon defaults are unchanged.

The mandatory cancellation gate uses exactly two V2 lane bindings. Each lane
first consumes one bounded `HoldUntilCancel`; the second lane then consumes one
ordinary text action on the same durable agent after both exact prompt ids have
been canceled. The oracle requires one `agent.prompt_terminated` and one
provider cancellation acknowledgement per held id, zero accepted or durable
provider terminals for canceled ids, one successful terminal for the fresh id,
exact scenario consumption, and no timeout trace. The harness clears matching
ordinary-inference runtime `DispatchUncertain` ownership when cancellation
terminalizes the live prompt so queued work on that agent can advance;
standalone-compaction ownership is preserved. The cancellation terminal remains
transient and late provider responses remain discarded. This proves
warm-process liveness, not crash-exact cancellation persistence.

This boundary validates extension supervision, CBOR lifecycle, model routing,
prompt assembly, provider-event validation, one real tool continuation, typed
terminal error projection, exact cancellation with same-agent post-cancel
liveness, bounded provider stalls, fatal
provider-disconnect handling without restart, lane isolation, durable session
projection, clean restore/shutdown, and the spawned public terminal's completed
tool projection across one quiescent cold resume. S1 specifically spends four
main-lane actions and two worker-lane actions, producing exactly five main
provider turns and two worker provider turns, to prove that a completed,
production-started durable worker cold-restores as an independently addressable
idle conversation; its daemon-lifetime automatic watch does not restore.
S2 repeats the four-main/one-worker Boot A budget in a fresh fixture. Boot B
spends two main turns on the exact explicit watch pair, three main turns on the
coalesced ordered watch notifications, and one worker turn on direct fresh
input: five main and one worker turn exactly. Extra prompts fail scenario
consumption.
S3 repeats S1's four-main/one-worker Boot A setup, then creates one promptless
ephemeral worker. Boot B spends exactly one fresh main and one fresh durable-worker
turn, for five main and two worker turns across both boots and six lane actions
total. Probes for the unloaded and vanished ephemeral identities must produce no
provider prompt or action.
S4 consumes two sequential start pairs and two three-record automatic-watch
actions in Boot A: eight main turns and one turn in each distinct worker lane.
Boot B consumes one fresh turn per worker in reverse creation order, with no
main turn. Exact lane-local continuations and per-agent durable suffixes reject
lane rebinding and cross-agent transcript leakage; roster rows are compared as
an ID-keyed set rather than by RPC order.
S5 consumes three main turns and one held worker turn before the synchronized
crash. Boot B consumes exactly two main turns for explicit watch recreation and
no worker turn; Boot C consumes no provider turn. The decoded fake cursor
remains at one consumed worker action throughout, while the durable worker
journal retains the one unfinished dispatch checkpoint. This proves
conservative harness recovery at the established cut, not exactly-once external
work or crash-transactional fake cursor recovery.
S6 uses two lanes and compact 1,259-byte scenario JSON: three main actions and two
worker actions. Boot A consumes three main turns and one worker turn before four
matched actions; Boot B consumes one worker turn; Boot C consumes none. Extra
repair events, terminals, starts, provider prompts, or actions fail closed.
S7 consumes nine of ten actions before its crash: eight main turns and one turn
in each worker lane. Two no-input resumed generations consume no provider turns;
the first owns the sole durable repair and both own one exact uncertain-worker
warning. The second snapshot equals the first before one explicit repair-worker
continuation consumes the tenth action. Exact checkpoint bindings, per-agent
journals, and provider budgets reject warning-to-model delivery, lane rebinding,
repair leakage, or automatic redispatch.
S8 uses two lanes and five scenario actions. Headless Boot A consumes three main
actions across exactly four main provider turns and one worker action/turn.
Universal PTY Boot B replay consumes nothing, then one explicit worker follow-up
consumes the fifth action and exactly one worker provider turn; the main consumes
zero Boot B turns. Stable IDs from typed creation facts drive worker-to-main-to-
worker `/agent switch` transitions without a picker. The VT oracle covers only
selected restored transcripts, the completed `agent_start` row never becoming
pending, and fresh worker transcript ordering. Side replay boundaries, exact
replayed `agent_start` lifecycle, post-boundary directed current/history rosters,
typed two-agent store record counts/prefixes/suffixes, the fake checkpoint, and
bounded process-group/socket cleanup reject replay work, lane rebinding,
cross-agent routing, and partial cleanup.
The peer-navigation case requires the authenticated bare delivery to report one
auto-started recipient, observes its complete stats snapshot as
`active_auto/running`, and waits for the correlated hold-ready notice broadcast
later to render on the target PTY. It sends the real Ctrl-J binding exactly once while the
correlated hold remains live. The selected prompt must name that recipient;
exact cancellation then reaps the hold without a timeout. This covers
first-agent navigation only; it does not expand peer trust, delivery, or crash
guarantees.
Sequential error then success is two
explicit user turns, not provider retry evidence. It is not evidence for
provider-builtin, upstream request/parsing, ChatGPT/WebSocket fidelity,
production retry scheduling, crash-exact replay, universal packaging beyond
the exact Gate 1 CLI and Gate 2 bundled core-shell components, or
broad terminal rendering fidelity. Live/VCR and transcript-replay fixtures remain separate.

The fake Provider submits prompt, update, and terminal output through explicit transient
`provider.*_reported` events. Assertions consume the harness-canonical execution facts,
so deterministic scenarios exercise the same commit-before-correlation boundary as
production providers.

Refines [ARCH-tau-e2e-tests](ARCH-tau-e2e-tests.md).
