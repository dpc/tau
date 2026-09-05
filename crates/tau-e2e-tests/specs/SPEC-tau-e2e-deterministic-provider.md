# SPEC-tau-e2e-deterministic-provider: Deterministic provider acceptance contract

## Record justification

This contract is necessarily distributed across fixture configuration, fake-provider grammar/checkpointing, production-tool injection, durable stores, socket observation, and deterministic CI.

## Closed sibling-shell concurrency

One closed V2 core-shell scenario emits exactly four sibling `shell` calls in
one canonical provider terminal. It runs in both capability modes: the positive
case advertises parallel calls, while the violating-response control retains
one-call guidance and still requires lossless aggregation. Each fixed command
uses the preflight-resolved Cargo-built helper path, synchronizes through private cwd marker
files retained for the fixture lifetime, and records monotonic timestamps around
a three-second sleep. One bounded plural exact `wait` call collects the real
background results in request order after the commands finish in reverse order. The oracle
requires all requests and starts before the first placeholder, exact
call/wait/result correlation without extras, common overlap, and a makespan
below six seconds. It does not alter runtime scheduling.

One separate closed mixed-result scenario emits a fixed successful core-shell
`shell --version` call, a fixed failing relative `workdir` call, and one bounded
plural exact wait in the same provider terminal. Its continuation requires one
aggregate containing the success and error members in request order without
fail-fast behavior.

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
Under this closed fixture convention it projects fixture-authored expected
HumanUi text and compares provider bytes without decoding; raw `</user>` and raw
`&lt;/user&gt;` intentionally collide in the one-way provider form. Every other
fixture string remains literal. A future non-Human fixture that needs the
same literal syntax requires a typed scenario distinction rather than heuristic
inference. V1's first text and tool-call actions additionally require the exact
HumanUi envelope, so the always-on deterministic lane fails if interactive
prompt projection disappears or changes structure.

The V2 grammar includes one closed two-lane production-message exchange. Its
message-only main binds one test-driver-created tool-free idle worker through
typed creation identity, then checks a correlated compact sender result. The
received fact causes a payload-free wake; its one later recipient provider
request contains exactly one canonical inbound wrapper. The gate does not
combine restart, watch, wait, peer, or branch behavior.

The gated provider-context placement exception holds one target response while
the production `message` tool commits a typed receipt and the configured
test-dummy raw-message tool commits one activating canonical message fact.
Only then does a named barrier release either ordinary text or parallel dummy
calls. The sender-side release has two legitimate scheduler shapes: it may
arrive as the next prompt after the raw tool-result continuation, or it may be
steered into that continuation while the tool still completes. The latter shape
requires the exact retained `prior raw-call input, release input` suffix, one
release overall, the exact successful raw tool result, and the adjacent matching
barrier action; absent, duplicated, interposed, reordered, or non-inference
content fails before cursor mutation. The target must observe the response, and
for tool calls the complete aggregate, before both deferred inputs in exactly
one successor prompt.

The fixture uses fresh private config, state, session, process-runtime, and
artifact directories. Its embedded harness socket and discovery catalog remain
below that process-runtime directory.
It disables every unrelated built-in extension and normally enables only the
no-side-effect `tau-ext-test-dummy` success mode. Gate 2 is one controlled
exception: the exact universal `component ext-shell` uses separate closed
surfaces: `workdir`/`edit` for cold resume and `shell`/`wait` for sibling
concurrency. S1 is the other: its main role
exposes only the production harness-owned `agent_start` built-in while its
worker role exposes no tools. The isolated production-message fixture instead
exposes only `message` to its main and no tools to its test-driver-created idle
worker. S2 adds only production `agent_watch` to that main role. S3 reuses S1's
exact roles and grammar; its promptless ephemeral agent
and typed unloaded-worker store records consume no fake-provider action. S4
instead configures two distinct tool-free worker roles and keeps only
`agent_start` on the main. S5 reuses S2's two-role tool surface for one
synchronized interrupted-worker restore. S6 instead exposes only
`restart_test_dummy` in exact `hold_until_success_release` mode to the worker
while the main retains only `agent_start`. S7 uses one main, two tool-free worker roles,
and one repair-worker role exposing that same sole dummy. Its main consumes the
existing two production-start pairs; one fixed durable UI creation supplies the
repair worker without extending the fake grammar. S8 reuses S1's fixed
production-main and tool-free worker roles. Its headless Boot A enables only the
fake-provider extension and exposes only harness-owned `agent_start` to the main;
its universal PTY Boot B preserves that exact extension/tool surface. The fake
has no network,
authentication, shell, evaluation, child-spawn, prompt-control, environment
control, or arbitrary fixture-file behavior.
The current-status policy scenario is another closed exception: one main role
exposes only harness-owned `status` plus `restart_test_dummy`, installs the
production built-in handler before its first prompt, and advertises parallel
tool support only for that scenario.
The separate peer-navigation PTY case starts with no agent, exposes no tools,
and authorizes one exact external message through a fixture-owned same-process
callback endpoint. Its sole provider action is a bounded hold used to inspect
the live navigation interval.
The live dual-PTY attach cases reuse the one-lane public-PTY binding exception.
One case permits a sole text action to bind the harness-minted `ui-prompt-*`
correlation and attaches after completion. Tool cases permit exactly two fake
provider actions around one deterministic `restart_test_dummy` invocation:
completed attach uses immediate success, while pending attach waits on the
fixture-private authenticated release socket until both PTYs show the same
pending row. The interruption case separately permits one bounded
`HoldUntilCancel` and one exact cancellation. Public terminal snapshots must
agree on selected-agent and pending/settled status while typed lifecycle facts
prove runtime; attachment itself consumes no provider action or tool invocation.
The snapshot assertions do not claim an absence of intermediate redraw flicker.

The one-shot `core_resume` cases
`implicit-fresh-create`, `prompt-stdin-literal-colon`, `prompt-stdin-success`,
`prompt-stdin-provider-failure`, `prompt-stdin-piped-terminal-controls`, and
`prompt-stdin-pty-terminal-controls` use that same closed single-lane
`ui-prompt-*` binding exception. The first case proves a genuinely fresh
terminal's first prompt creates its agent without an explicit command. The last
two send one exact hostile semantic response through the real prompt-stdin
process: the piped case owns raw nonterminal byte/framing assertions, while the
PTY case owns terminal sanitization and drains the child and PTY reader before
inspecting raw capture.

The two-agent attached-UI case first completes the deterministic main/worker
scenario and correlates both stable IDs and idle readiness through typed facts.
Each UI then selects both IDs independently. Exact fake traces, typed provider
and stats observations, and equal typed durable-event snapshots prove that
selection performs no model work or typed durable-event mutation.

Hermetic embedded and daemon launches bypass ambient Tau startup-role,
role/config, extension, and secret environment transports, retain that policy
for runtime settings reloads, then check an exact extension-name allowlist before
any process starts. This is a deterministic-test exception to
normal interactive startup availability in
[SPEC-tau-harness-extension-lifecycle](../../tau-harness/specs/SPEC-tau-harness-extension-lifecycle.md);
embedded launches do not scrub the ordinary child OS environment. Spawned daemon
acceptance clears it and supplies private HOME/XDG roots plus a fixed locale.
The provider-context placement fixture separately exposes `message` and
`provider_context_raw_message` to its main and `restart_test_dummy` to its
worker; all three are closed test-only actions.

`ScenarioV1` remains the closed phase-one grammar. `ScenarioV2` adds at most
eight exact `ctx_id` lanes with independent bounded cursors. It supports typed
terminal errors, exact cancellation holds with hard timeouts, deliberate
disconnect, and named barriers whose participants must all submit before any
lane completes. It also has one narrow adjacent action pair for the allowlisted
`restart_test_dummy` empty-argument call and exact successful result; arbitrary
tool names, arguments, and results remain outside the grammar. A separate sole
lane four-action exception permits exactly distinct
`call → disconnected-error repair → call → success` pairs: its first call must
retain the exact harness disconnected diagnostic in the repair, and its second
call must retain exactly that repair plus its own normal success. This exception
exists only for the real supervised test-dummy respawn acceptance; every other
V2 shape retains the sole adjacent pair limit. A separate sole
typed-image lane permits one fixed empty-argument dummy call, its one native 1×1
PNG result, and one clean-resume continuation. The fake accepts that result only
when its call identity, type, dimensions, detail, canonical bytes, and BLAKE3
digest are exact and it accepts the resumed prompt only when that image remains
once. Its durable snapshot independently checks the same digest and balanced
call/result round; trace and terminal metadata remain byte-free. The one closed
image fixture refines the root
[SPEC-typed-image-tool-results](../../../specs/SPEC-typed-image-tool-results.md).
The one closed reactive-recovery sequence requires an ordinary `ContextOverflow` with empty
semantic output and `context_window_exceeded`, immediately followed by
`ReactiveOpaqueCompaction` and one `ReactiveCompactedOpaqueText` continuation.
The fake verifies inference versus standalone operation, the rejected prompt's
validated pre-cut compactor request, and the opaque replacement plus retained
overflow-prompt suffix continuation context;
the acceptance test separately correlates the durable failed terminal, reactive
start, replacement, and the setup, rejected, compaction, and continuation
prompt-start checkpoints. S6's separate
closed repair sequence accepts the same sole dummy call only when followed by
the exact synthetic interrupted error status and diagnostic, terminalizes that
next explicit worker continuation only while the balanced call/error pair
remains in context. S1 adds one
distinct adjacent `AgentStartCall`/`AgentStartResult` pair; S4 raises that
closed bound to two unique adjacent pairs. The fake requires
exact bounded instruction, required role, call id, production tool
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

One separate sole-lane two-action shape exercises the output-length contract.
Its first action emits exactly one nonempty full-reasoning `Length` response,
with closed Chat Completions backend metadata and no assistant message or tool
call. Its adjacent successor accepts only the exact initial HumanUi projection,
retained full reasoning, and shared harness-internal continuation instruction in
that order, then emits one normal answer. The pair cannot mix with any other
action or lane. The acceptance oracle requires exactly two provider requests,
one continuation steer and owner, two independently accounted terminal facts,
and no third request.
The pair's matching `report_usage` flags select either fixed nonzero, distinct
per-response usage or no usage at all. The live oracle checks each captured rate
and cost increment plus their aggregate exactly once; the cold-cut oracle retains
the absent-usage case.
The restart oracle alone enables a feature-gated one-shot barrier after the
planned response or continuation steer has completed durable publication and
before its next post-commit action. That oracle selects two cuts from the shared
four-cut fixture protocol; the deferred-receipt oracle selects the typed-receipt
and next-provider-response cuts. The test daemon accepts those exact four values
plus an absent Unix socket directly below its private harness-state root. At the
matching hook it reports the exact cut, daemon process identity, and fixed
durability deadline before waiting. It then reports durable success, durability
timeout, or unavailable/failed persistence ownership with measured elapsed
time; durable success blocks the event loop and relies on the existing
process-group `SIGKILL` owner. The observer uses one bounded protocol to
distinguish a missing hook, premature daemon exit, and transport or protocol
failure from those producer outcomes. No production configuration or durable
fact represents the barrier, and the protocol does not claim to repair an
underlying crash-cut miss.

V1's closed current-status sequence emits Working plus one dummy call in either
provider order, optionally substitutes one rejected status state, validates the
resulting reminder behavior, performs another Working-state tool round, attempts
a final, and then requires Done or Blocked before the accepted final. The fake
matches typed tool results/errors and exact reminder text; it cannot generalize
to other status values, tools, or call sequences.

S1 also adds one bounded `WatchNotifications` action containing one to four
typed `Response` or `Prompt` records. Each provider prompt consumes and validates
the complete already-delivered queue prefix for the current closed action; an
incomplete prefix returns fixed text without advancing the lane action. The action
requires the retained child identity, exact sender/recipient, kind, and content.
It also checks the exact ordered, escaped model-visible prompt projection. Only
records for the current closed action enter the queue; unrelated, reordered, and
excess live traffic fails before admission. Replayed deliveries cannot populate
this live queue. Response and Prompt records validate exact content.

S2's closed `WatchNotificationChains` action requires the explicit watch to
create exactly one non-model initial Unreported work-status snapshot with a new
nonempty subscription ID distinct from Boot A. Its one fresh direct worker turn
then yields exactly one prompt notification and one final-response notification
in that order. The initial snapshot consumes no provider action.

A barrier is normally the lane's sole action, appears once per distinct
participant lane, and has one consistent bounded participant count, preventing
same-lane and cyclic barrier plans. The provider-context placement sender is the
sole exception: its closed raw-result action may consume itself and its adjacent
matching barrier from one coalesced prompt. That transition advances and
checkpoints both actions atomically before joining the already-waiting barrier;
the later-prompt shape advances only the raw-result action and leaves the
barrier pending. Every other declared participant must already be staged before
the coalesced transition; missing or insufficient readiness rejects before lane
binding, cursor advance, or checkpoint mutation. Both consumed actions retain
distinct trace records. Initial `ctx_id` binds an agent to one lane.
The public terminal UI supplies no initial `ctx_id`, so an unbound first prompt
may select the sole configured lane. S1's production-started worker also has no
initial `ctx_id`; neither do S4's two production-started workers. Their exact
first user text must select exactly one unconsumed, unbound lane. Zero or
multiple candidates fail closed. Every selected binding is immutable and
checkpointed. Other multi-lane agents still require an exact `ctx_id`.
Continuations cannot change that binding. The fake subscribes
only to live prompt/cancel/watch traffic, so restored event replay cannot
consume actions.

The cold-resume core-shell action family can set only relative `project` and
edit only `resume-sentinel.txt` with two fixed line-range shapes. The separate
sibling-concurrency family is the exact three-action call/wait/result sequence
in [Closed sibling-shell concurrency](#closed-sibling-shell-concurrency); no
extra lane or action may accompany it. Both families require
successful, exactly correlated continuations.

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
The fixture never sends the authenticated release, so the existing release
arbitration keeps the invocation live until process teardown rather than racing
the crash cut against an elapsed terminal deadline. After SIGKILL, the fixture
removes the dead generation's private release socket before starting Boot B.
Its compact 1,085-byte scenario JSON contains two main actions and two worker
actions.
Boot A requires exactly one correlated worker `tool.request` followed by canonical
`tool.started` in the typed execution-restore stream and one live readiness fact
before the process-group kill. Boot B requires one durable
`provider.tool_error` with the full restart/possible-side-effect diagnostic,
followed by one derived live nonsemantic `tool.error`, no live dummy redispatch,
and one explicit worker continuation whose
complete tool-result context is exactly that balanced error. Boot C receives no
input and must add no repair; its current/history membership, execution restore,
current-agent journals, and separately loaded worker journal equal Boot B.

S7 uses four lanes and compact 2,178-byte scenario JSON: five main actions, one
quiescent-worker action, one uncertain hold, and the two-action dummy repair
pair. Boot A consumes eight of nine actions, checkpointing exact cursors
`[5, 1, 1, 1]`, four immutable lane bindings, and two contiguous
production child ordinals before the combined crash cut. Boot B and the
no-input portion of Boot C consume no provider action. Only the uncertain worker
may own the exact per-generation warning, only the repair worker journal may
gain Boot B's provider error, and Boot C's durable snapshot must equal Boot B.
One explicit repair-worker continuation after that equality check advances only
its cursor to `[5, 1, 1, 2]`; the uncertain dispatch remains unfinished.

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

The local-summary-compaction acceptance grammar is equally narrow. Only a V2
scenario containing one of its dedicated standalone actions causes the fake to
publish `supports_standalone_compaction`; every other scenario remains opted
out. The actions accept a harness-owned compact prompt, one bounded private
typed local narrative output, one fixed canonical opaque compaction item,
terminal provider error, or exact cancellation hold. The opaque action's one following
ordinary action runs only after a clean daemon restart and requires exactly one
`ContextItem::Compaction` with the fixed raw provider JSON while the discarded
user text is absent. The local narrative action emits the same dedicated
private typed output as the production extension, requires the exact model final
text as one synthetic user checkpoint, stops Boot A, then validates
byte-identical durable replay, removed source context, and
ordinary continuation in Boot B. This proves harness transaction, typed-local
conversion, durable replacement, opaque and local-summary cold replay, and
continuation semantics through the provider extension seam; it does not expand
the grammar into a compaction outcome matrix. Shared `tau-provider` tests own
the trailing instruction and limits, the Chat Completions adapter owns its
private cache-aligned ordinary-prefix wire lowering, and provider-builtin tests own public
Responses fallback dispatch and validation.

One separate closed lifecycle scenario runs a successful deterministic dummy
tool round, finishes on its distinct ordinary continuation above an inferred
Done outer-finish threshold, commits one automatic opaque standalone
replacement, restarts cleanly, and requires the next ordinary inference to see
the replacement without the removed source prompt. It owns that exact
cross-process lifecycle and replay composition, not other statuses, terminal
classes, provider shapes, triggers, retry policies, or a general outcome matrix.

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
S4 consumes two sequential start pairs and two automatic-watch actions in
Boot A: eight main turns and one turn in each distinct worker lane.
Boot B consumes one fresh turn per worker in reverse creation order, with no
main turn. Each accepted worker prompt produces live non-replay `active` stats,
and the same-daemon roster retains `active` after both workers return idle. Exact
lane-local continuations and per-agent durable suffixes reject lane rebinding and
cross-agent transcript leakage; roster rows are compared as an ID-keyed set rather
than by RPC order. An input-free Boot C consumes no provider turn or action and
reports both delegated workers as `active_auto`, proving the accepted prompts'
implicit navigation writes are not restored from durable history.
S5 consumes three main turns and one held worker turn before the synchronized
crash. Boot B consumes exactly two main turns for explicit watch recreation and
no worker turn; Boot C consumes no provider turn. The decoded fake cursor
remains at one consumed worker action throughout, while the durable worker
journal retains the one unfinished dispatch checkpoint. This proves
conservative harness recovery at the established cut, not exactly-once external
work or crash-transactional fake cursor recovery.
S6 uses two lanes and compact 1,085-byte scenario JSON: two main actions and two
worker actions. Boot A consumes two main turns and one worker turn before three
matched actions; Boot B consumes one worker turn; Boot C consumes none. Extra
repair events, terminals, starts, provider prompts, or actions fail closed.
S7 consumes eight of nine actions before its crash: five main turns and one turn
in each worker lane. Two no-input resumed generations consume no provider turns;
the first owns the sole durable repair and both own one exact uncertain-worker
warning. The second snapshot equals the first before one explicit repair-worker
continuation consumes the ninth action. Exact checkpoint bindings, per-agent
journals, and provider budgets reject warning-to-model delivery, lane rebinding,
repair leakage, or automatic redispatch.
S8 uses two lanes and five scenario actions. Headless Boot A consumes three main
actions across exactly four main provider turns and one worker action/turn.
Universal PTY Boot B replay consumes nothing, then one explicit worker follow-up
consumes the fifth action and exactly one worker provider turn; the main consumes
zero Boot B turns. Stable IDs from typed creation facts drive worker-to-main-to-
worker `:agent switch` transitions without a picker. The VT oracle covers only
selected restored transcripts, the completed `agent_start` row never becoming
pending, and fresh worker transcript ordering. Side replay boundaries, exact
replayed `agent_start` lifecycle, post-boundary directed current/history rosters,
typed two-agent store record counts/prefixes/suffixes, the fake checkpoint, and
bounded process-group/socket cleanup reject replay work, lane rebinding,
cross-agent routing, and partial cleanup.
The distinct `s8-agent-trace-live-descendant-companion` fixture reuses S8's two
roles and exact tool surface but owns a separate six-action budget. Boot A
consumes the same first four actions. Resumed Boot B routes exactly two tool-free
worker prompts: one held/cancelled turn for live descendant tracing, then one
successful follow-up. It requires empty tool snapshots, zero main turns, exactly
two worker turns, no tool execution, no provider mismatch, and exact six-action
consumption. This companion does not alter S8's five-action restore contract.
The peer-navigation case requires the authenticated bare delivery to report one
auto-started recipient, observes its complete stats snapshot as
`active/running`, and waits for the correlated hold-ready notice broadcast
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
