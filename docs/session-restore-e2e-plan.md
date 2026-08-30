# Session-restore end-to-end test plan

## Purpose and status

The intended checked-in path is `docs/session-restore-e2e-plan.md`; links are
relative to that path.

This plan stages end-to-end coverage for restoring a session containing a main
agent and durable worker agents. It records test work, not new product behavior.
Implement scenarios in order unless an earlier scenario exposes a contract gap.

The first goal is a small, deterministic proof that Tau restores a main agent
and a quiescent, currently loaded worker created through the production
`agent_start` path. Later stages add membership variants, runtime-only state,
concurrency, interrupted inference and tools, repeated resumes, and the public
terminal boundary.

## Current contract

Tests must keep these distinct:

- `agent.started` is the immutable creation fact at sequence zero of one agent
  journal. It records stable identity, parent, role, initial metadata, display
  name, and persistence. Restoring an agent replays this fact; it does not create
  or start that agent a second time.
- `session.agent_loaded` and `session.agent_unloaded` are session membership
  facts. Cold resume rebuilds routes only for the composed current durable loaded
  set. An old `agent.started` fact does not make an unloaded agent current.
- `AgentRuntimeState::{Idle, Running}`, watch edges, and navigation overrides are
  daemon-lifetime state. A restored durable route is live, but that alone does
  not mean model work is running. Cold restore recomputes the ordinary
  `active` and delegated `active_auto` navigation defaults. A fresh accepted
  direct human prompt makes an existing restored worker `active` within that
  daemon lifetime; a subsequent cold restore recomputes `active_auto` rather than
  replaying the implicit write.
- Ephemeral agent transcripts and membership survive same-daemon catch-up but not
  cold resume. Durable agents remain durable in an ephemeral session; these are
  separate policies.

Creation commits before first membership publication during live creation. A
late-subscriber or resumed-session catch-up has a different observation shape:
it announces a replay-marked current membership snapshot and then replays each
loaded agent journal. Tests must not infer durable commit order from catch-up
delivery order.

Replay must be observational. It must not invoke a tool, consume a fake-provider
action, deliver an old watch response as new input, or append another creation or
membership fact. Each restored agent gets a non-replay
`agent.replay_complete`; buffered live traffic follows the non-replay
`session.replay_complete`.

An inference dispatch checkpoint with no durable terminal response is
dispatch-uncertain and is not resent automatically. An unresolved foreground
tool call is closed conservatively with a durable synthetic error and is not
rerun. These are distinct recovery cases.

## Existing coverage and remaining gaps

The deterministic fake-provider suite already proves a clean, quiescent
same-agent resume and preservation of its provider lane cursor. The Unix
`core_resume` gate runs the exact `tau` binary under a PTY and proves one
completed dummy-tool row remains terminal after `tau resume`; its replay-aware side
observer and typed stores also prove stable identity and prefix durability.
`core_shell_resume` separately proves reconstruction of production core-shell
per-agent workdir state.

Harness tests cover multi-agent rehydration, delegated-role/default-navigation
reconstruction, missing creation journals, non-default-agent tool repair,
agent-owned background notices, unloaded-recipient classification, and dropping
watch topology on restart.

S1 creates a production worker, cold-restores main and worker together, and
exercises both restored routes. S2 repeats that setup and explicitly recreates a
non-persistent watch with a fresh subscription. S3 composes current durable,
historically unloaded, and process-local ephemeral membership across a clean
restart. S4 restores two distinct durable workers, activates them in reverse
creation order, and proves that lane ownership, transcript suffixes, and ID-keyed
rosters do not depend on completion or RPC row order. S5 synchronizes a held
worker's durable dispatch checkpoint, decoded fake cursor, and live readiness
before a process-group kill, then proves two resumes remain dispatch-uncertain
without automatic worker submission or old-watch restoration. S6 kills after a
worker's exact durable foreground-tool request/start pair and closed dummy
readiness, proves one conservative repair and balanced explicit continuation,
then proves the next cold resume adds no duplicate. S7 composes quiescent,
dispatch-uncertain, and interrupted-tool state in one four-agent session and
proves a second no-input resume adds no cross-agent work or durable suffix. S8
is implemented by
`core_resume::multi_agent::public_terminal_cold_resume_selects_main_and_worker`.

## Test boundaries and oracles

Use the cheapest boundary that proves each claim:

1. **Headless deterministic daemon, default.** Run the real harness daemon and
   supervised fake provider over the normal socket protocol. Use this for exact
   lifecycle, replay, routing, isolation, and recovery assertions.
2. **Typed durable stores.** Open the `SessionStore` and global `AgentStore`;
   read the target membership and execution-restore streams and every relevant
   per-agent journal before and after restart. These are authoritative for exact
   record counts, sequence-zero creation, membership composition, parent/role
   facts, unchanged prefixes, and new suffixes.
3. **Replay-aware side UI.** Subscribe with separate exact historical and live
   selectors. This is authoritative for replay flags, recorded timestamps,
   per-agent and session boundaries, and absence of old live execution.
4. **Directed roster RPC.** After the session boundary, query the current and
   history scopes. This is authoritative for `live`/`unloaded` classification,
   runtime state, persistence, creation-fact enrichment, and recomputed
   navigation mode.
5. **Fake-provider trace and cursor.** Require exact lane/action consumption.
   Zero matches before intentional post-resume activation proves replay did not
   wake the provider.
6. **Spawned public PTY, narrowly.** Add one later `tau resume` scenario for terminal
   selection and transcript presentation. Pair every VT assertion with the side
   observer and stores; pixels alone are not lifecycle evidence.
7. **`tau dev tmux`, exploratory only.** Use it for a final real-like manual
   smoke test. It is not a CI oracle or a replacement for typed assertions.

Every automated fixture uses fresh private HOME/XDG, config, runtime, provider
state, session, and artifact roots; an exact extension allowlist; bounded
deadlines; process-group cleanup; and retained bounded artifacts on failure.
The Nix deterministic lane remains network-denied. Synchronize shutdown or
forced termination on semantic events and durable reads; never use sleeps as
scenario synchronization or evidence.
Teardown may use bounded waitpid/socket/lock/process-reap polling; elapsed time
is a cleanup deadline, not a scenario oracle.

## Fixture work, added only with its first consumer

Keep the fake provider closed and data-driven:

- Extend durable snapshots from one assumed agent to a map keyed by `AgentId`,
  exact session membership history, and the session's execution-restore stream
  from `SessionStore::session_restore_events`. Preserve the convenient
  one-agent helpers for existing gates.
- Add an explicit deterministic main role and worker role using `fake/test`.
  Configure the main's exact internal-tool surface for `agent_start` and, when
  S2 lands, `agent_watch`; keep the worker's surface minimal. Assert the
  generated role/model/tool configuration before spawning processes.
- Add a closed adjacent V2 `AgentStartCall`/`AgentStartResult` pair.
  `AgentStartCall` matches exact user text and emits only the built-in
  `agent_start` tool with exact instruction, required role, and call
  ID. `AgentStartResult` accepts only the correlated successful result,
  validates harness-minted `self_agent_id` and `sub_agent_id`, and emits its
  configured terminal response.
- Add closed actions that match bounded ordered model-visible watch-notification
  chains by symbolic child slot: received user prompt and final response. Each
  provider prompt consumes and validates the complete already-delivered queue
  prefix; an incomplete prefix emits fixed text without advancing the action,
  and multiple accepted records may coalesce in one prompt. Match the typed kind,
  exact expected content, subscription correlation, and harness-minted child ID. S1 needs these actions because `agent_start`
  automatically watches the child; S2 needs them after recreating the watch.
- Add a bounded, one-way lane release only if needed to make the child complete
  after the main has terminalized its `AgentStartResult` continuation. Validate
  dependencies as an acyclic graph, enforce a hard deadline, and do not expose a
  general scheduling language. This removes a race between the main continuation
  and its child watch notification without using sleeps.
- Add a bounded worker-lane binding rule for an `agent_start` child whose first
  prompt has no UI `ctx_id`. Bind from unique exact scenario data such as its
  initial instruction, fail on ambiguity, and checkpoint the resulting immutable
  agent-to-lane binding. Do not add arbitrary tool, prompt, file, or environment
  interpretation.
- Teach the observer to retain exact selectors needed by the scenarios,
  including `agent.started`, `session.agent_loaded`,
  `session.agent_unloaded`, `agent.stats_updated`,
  `agent.watches_updated`, `agent.message_received`, `harness.notice`,
  `agent.inference_dispatch_started`, `agent.replay_complete`, and
  `session.replay_complete`. Do not replace these with broad category
  subscriptions.
- Add a controlled hold only when the first interruption scenario needs it.
  The hold must expose a semantic readiness event, have a hard timeout, and make
  cleanup reap its worker.

Place S0-S7 in the existing `deterministic_provider` integration-test target,
reusing its daemon support; place S8 in `core_resume`. If implementation instead
adds a test binary, add that binary explicitly to
`flake.nix`'s focused `ci.tests` post-check selection and update
`crates/tau-e2e-tests/README.md`.

Before implementing each scenario, record its lane/action/encoded-size budget.
Current V2 limits are eight lanes, eight actions per lane, and 16 KiB total.
Count every `agent_start` call/result continuation and every watch-driven main
turn, not only visible user turns. Split a scenario that exceeds a bound; do not
raise a fixture limit just to preserve this ladder.

S1 budgets two lanes: exactly four main lane actions (start call/result, the
post-completion watch action, and Boot B's fresh prompt) and two worker lane
actions. The ordered response record uses the closed queue-prefix batching rule, so the
typed execution oracle requires exactly four main provider turns and two worker
provider turns across both boots. S2
omits S1's fresh main prompt and uses exactly six main lane actions: the Boot A
start pair and watch action, then the Boot B watch call/result and two-fact
prompt-before-response watch action. It uses exactly two worker actions. Boot A
spends three main and one worker provider turn; Boot B spends four main and one
worker turn, totaling seven main and two worker turns. Add a focused failure if either
setup produces an unexpected extra provider prompt; do not silently add an
unbounded action.

S3 reuses S1's two lanes and compact 1,116-byte scenario JSON: four main actions
and two worker actions, below the 16 KiB ceiling. Boot A spends three main and one
worker provider turn; Boot B spends one main and one worker turn, totaling four
main and two worker turns. Promptless ephemeral creation, typed store seeding,
roster queries, and absent-route probes consume no fake-provider action.

S4 uses three lanes and compact 1,704-byte scenario JSON: exactly six main
actions (two adjacent start call/result pairs and two ordered response watch
actions) and two actions in each distinct worker lane, below the 16 KiB ceiling.
Boot A spends six main and one provider turn per worker. Boot B activates only
the workers, in reverse creation order, and spends one provider turn per worker,
totaling six main and two turns per worker. Roster queries and replay consume no
fake-provider action. Boot C receives no input and consumes no provider action
while proving cold restore recomputes both delegated workers as `active_auto`.

S5 uses two lanes and compact 1,026-byte scenario JSON: exactly four main
actions (the Boot A start pair and Boot B's explicit watch call/result pair) and one worker hold action, below the 16 KiB
ceiling. Boot A spends two main and one worker provider turn. Boot B spends
two main turns and zero worker turns; Boot C spends zero provider turns. The
initial dispatch-uncertain watch snapshot, roster queries, replay, and the
synchronized crash cut consume no additional fake-provider action.

S6 uses two lanes and compact 1,085-byte scenario JSON: exactly two main
actions (Boot A's start pair) and two worker
actions (the foreground dummy call and Boot B's repair-aware continuation).
Boot A spends two main turns and one worker turn before three actions have
matched. Boot B spends one worker turn after the eager repair pair; Boot C spends
zero provider turns. Roster queries, typed store reads, the crash cut, and repair
observation consume no additional fake-provider action.

S7 uses four lanes and compact 2,178-byte scenario JSON: five main actions, one
quiescent-worker action, one dispatch-uncertain hold action, and two repair-worker
actions. The main uses the existing two-start maximum for the quiescent and
uncertain workers; the fixture creates the durable repair child through one exact
`UiCreateAgent` request so the main lane remains below the existing eight-action
limit. Boot A spends five main turns and one turn per worker before eight actions
have matched. Boot B and the no-input portion of Boot C spend zero provider turns;
only an explicit post-Boot-C repair continuation spends the repair worker's final
turn. Roster queries, replay, repair/warning observation, and typed store reads
consume no fake-provider action.

Every new fake action, binding, or release primitive updates
`SPEC-tau-e2e-deterministic-provider.md`, `crates/tau-e2e-tests/SECURITY.md`,
grammar validation, and fail-closed mismatch/bounds tests in the same change as
its first scenario.

If the fake-provider checkpoint and harness journal cannot establish the
requested cut without ambiguity, narrow the claim. Do not describe the current
fake as a transactional backend acknowledgement protocol.

## Scenario ladder

### S0 — Single-main clean-resume baseline

Retain the existing quiescent clean-resume scenario as the basic control and
tighten its lifecycle oracle when the multi-agent helpers land.

Boot A creates one durable main, completes one text turn, reaches `Idle`, and
shuts down cleanly. Boot B resumes the same session, waits through all replay
boundaries, and submits one fresh targeted prompt.

Assert:

- the agent journal has exactly one matching `agent.started` at sequence zero;
- session history has exactly one durable `session.agent_loaded` and zero
  `session.agent_unloaded` records for the ID, and composed current membership
  contains the ID;
- Boot B replay exposes the same ID and creation fact, then reports the main as
  `live`, `idle`, `durable`, and `active`;
- no fake action matches during replay;
- Boot A's decoded typed durable records are a record-for-record prefix, and
  only the fresh Boot B turn forms the per-agent suffix.

### S1 — Quiescent main and completed worker

This is the first new scenario and the recommended first implementation.

In Boot A, the main's deterministic lane calls the production `agent_start`
tool. The worker gets a distinct exact lane, completes its instruction, and the
main consumes all resulting tool/watch-driven work. Wait until both agents are
loaded, have terminal provider responses, report `Idle`, and have no foreground
tools before a clean shutdown.

In Boot B, resume without submitting input. Wait for both agent boundaries and
the session boundary, then send one exact targeted prompt to the main and one to
the worker.

Assert:

- two stable agent IDs return, with exactly one `agent.started` in each journal
  and, for each ID, exactly one durable `session.agent_loaded`, zero
  `session.agent_unloaded`, and current composed membership;
- the worker creation fact retains `parent_agent = <main ID>`, the selected
  `role`, and no display name from the start request; replay does not
  append a second creation or membership fact;
- both routes and both transcripts are available after resume, the main is
  `active`, the worker is `active_auto`, and both initially report `Idle`;
- the pre-restart automatic watch edge is absent; the old worker completion
  appears exactly once as replay/transcript context, with zero live re-fanout
  and zero fresh provider input;
- replay consumes no fake action, then each fresh prompt consumes exactly its
  own preserved lane and appends only to its owning journal;
- per-agent replay boundaries precede the session boundary and all fresh work.

This scenario proves restoration, not persistence of the original
`agent_start` request's transient ownership. The completed worker remains an
addressable loaded conversation. The same governed lifecycle applies to a
completed explicit-parent typed `agent.start_request` without a `tool_call_id`;
parentless non-tool typed starts remain one-shot, and peer entrypoints remain
ordinary loaded agents.

### S2 — Explicit watch recreation after resume

Repeat S1's setup in a fresh fixture. With S2, add a closed adjacent
`AgentWatchCall`/`AgentWatchResult` pair. It binds a symbolic worker slot from
the validated `agent_start` result, permits only `enable: true` for that exact
harness-minted ID, and validates the correlated successful tool result and exact
sanitized text. The result does not contain watch subscription identity.

After Boot B proves there are no restored watch edges, have the main call this
exact action, submit a fresh direct user prompt to the worker, and let the worker
finish.

Assert one initial non-model work-status snapshot, one received-user-prompt
notification, and one final-response notification for the new subscription. The side observer captures the initial event's nonempty
subscription ID, proves it differs from Boot A's ID, and requires every later
edge to carry that new ID. Require the prompt notification before the response; do not impose any
stronger cross-stream order than the watch contract. Assert that neither the initial
snapshot nor any Boot A notification becomes fresh provider input. This keeps
watch restoration policy separate from transcript restoration.

Implemented by
`session_restore::cold_resume_recreates_explicit_worker_watch` using a fresh S2
fixture and the closed `AgentWatchCall`/`AgentWatchResult` grammar.

### S3 — Loaded, unloaded, and ephemeral membership

This scenario exercises restore composition, not a public unload operation.
Boot A creates the current durable main/worker pair through S1's production path
and creates an ephemeral agent through `UiCreateAgent`. Capture the main ID from
its live `agent.started`, verify the live durable/ephemeral roster, and shut down
cleanly. While no process owns the stores, seed a valid second durable worker
journal whose immutable creation fact names that captured main parent, plus a
valid session history containing one load followed by one unload for the seeded
ID. Validate all seeded records through typed store reads before Boot B resumes.

After cold resume, assert:

- current scope contains only the durable main and still-loaded durable worker;
- history scope also contains the unloaded durable worker as `unloaded`;
- the unloaded worker's durable `agent.started` still exists but no runtime
  route does;
- no ephemeral transcript, membership row, route, or replay boundary returns;
- no ephemeral agent journal/directory or durable session membership record
  exists before or after cold resume;
- a fresh prompt to each current durable agent still consumes the correct lane.

Do not fabricate a “loaded but never started” valid case. Membership without a
matching committed sequence-zero creation is corrupt input and belongs to the
existing fail-closed restore tests, with an E2E startup-failure case only if that
boundary later regresses.

Implemented by
`session_restore::membership::cold_resume_composes_loaded_unloaded_and_ephemeral_membership`
using the S1 production path, promptless `UiCreateAgent` ephemeral creation,
typed between-boot store seeding, exact current/history roster assertions, and
negative route probes.

### S4 — Multiple workers and ordering independence

Create at least two durable workers with distinct roles,
instructions, and fake lanes. Record the observed completion order without
treating it as authority, resume all agents, then activate the workers in an
explicit order different from creation.

Assert exact parent/role retention and, for every ID, one sequence-zero
creation, one durable load, no unload, and current composed membership. Require
one agent boundary per current member, no lane rebinding, no cross-agent
transcript suffix, and an ID-keyed roster result independent of the observed
runtime completion order. Treat RPC rows as a set; their order is not a
protocol contract. Require every accepted fresh worker prompt to publish a
non-replay `agent.stats_updated` with `active`, and require the current roster to
retain `active` after both turns become idle. After clean Boot B shutdown, an
input-free Boot C must consume no provider action and report both delegated
workers as `active_auto`, proving the implicit writes are daemon-lifetime only.
Do not use
`BarrierText`: its lane must contain no other actions and it cannot also support
the resumed activation in this scenario.

Implemented by
`session_restore::multiple_workers::cold_resume_multiple_workers_is_order_independent`
using two sequential production starts, exact distinct roles/instructions/lanes,
observed-but-nonauthoritative completion order, reverse-creation activation after
cold resume, ID-keyed durable and RPC assertions, live `active` snapshots and
retained roster modes after the accepted worker prompts, and an input-free second
cold resume that recomputes `active_auto` without provider work.

### S5 — Worker inference is dispatch-uncertain

Use a held worker provider action. In Boot A, wait until the worker's durable
`agent.inference_dispatch_started` checkpoint, the fake's decoded
`scenario-cursor.json` lane/action checkpoint, and provider hold readiness for
the same prompt are all observable, then kill the private daemon process group
without graceful shutdown. Provider readiness is a synchronized live
observation, not a durable backend acknowledgement.

In Boot B, assert that both main and worker routes restore, the worker exposes
the mandatory dispatch-uncertain `harness.notice`, and does not automatically
submit provider work. Recreate a fresh main-to-worker watch using S2's closed
action and require its initial provider snapshot to contain
`AgentWatchProviderState::DispatchUncertain` for the checkpointed
`agent_prompt_id`; restoring the old watch is not allowed. The main stays usable
and consumes no worker provider action. Stop Boot B without input to the worker,
then cold-resume once more and assert the same fail-closed worker state and zero
provider consumption.

This proves conservative harness behavior after a synchronized cut. It does not
prove whether an external backend performed work exactly once, and it must not
be used to claim crash-transactional fake-provider cursor recovery.

Implemented by
`session_restore::dispatch_uncertain::cold_resume_fails_closed_for_dispatch_uncertain_worker`
using the prompt-correlated three-way crash cut and two fail-closed cold resumes.

Recovery is a separately gated follow-on. The exact authority question is:

> For an ordinary inference restored from a durable dispatch checkpoint with no
> terminal response, which explicit user operation, if any, abandons or retries
> the uncertain work; must it reuse the checkpointed `AgentPromptId` or mint a
> new one; and when may later targeted input run?

Current `:retry` is not that operation: it addresses a live provider-owned
parked delayed retry and requires transient in-flight routing. Pause this
follow-on until the user or a maintainer explicitly confirms the semantics; do not
change recovery behavior to make the E2E terminate.

### S6 — Interrupted worker foreground tool

Implemented by
`session_restore::interrupted_tool::cold_resume_repairs_interrupted_worker_tool_once`.

Add one exact no-side-effect test tool mode that can acknowledge start and hold
before terminal output. Kill Boot A after the worker's canonical durable tool
request/start facts and before any terminal result. Decode the session's
execution-restore stream and require exactly one correlated `persist=true`
`tool.request` followed by one canonical `tool.started` for the worker/call ID
before killing. Before Boot B, prove the old process group, socket, and session
lock are gone.

The holding tool mode is closed fixture code, not an arbitrary tool. Its first
change must update `crates/tau-e2e-tests/SECURITY.md` and the owning fixture
documentation/tests, constrain configuration to the exact mode, expose bounded
readiness and timeout, join on cancel/shutdown, and provide no filesystem,
network, child-process, or environment-control capability.

On resume, assert that only the worker journal receives one durable
`provider.tool_error` with the restart/possible-side-effect diagnostic. Require
the correlated Boot B live repair pair—the durable provider error followed by
one derived non-semantic `tool.error`—and no pair on Boot C. The execution-restore stream
produces no second live `tool.started`; the main journal and lane are unchanged.
Add a closed repair-aware fake continuation that validates the exact call ID,
error status, and synthetic diagnostic before emitting a terminal response;
then require the worker's next explicit prompt to see a balanced tool round.
Stop Boot B with no further input and cold-resume as Boot C. Compare membership,
execution-restore, and agent streams and assert Boot C adds no duplicate repair.
Keep this separate from S5 because inference uncertainty and tool repair have
different authorities.

Because S6 changes `tau-ext-test-dummy`, update
`crates/tau-ext-test-dummy/specs/ARCH-tau-ext-test-dummy.md`,
`crates/tau-ext-test-dummy/TESTING.md`, and any required self-knowledge alongside
its focused closed-mode lifecycle tests.

### S7 — Mixed-state and repeated-resume isolation

Implemented by
`session_restore::mixed_state::cold_resume_mixed_state_is_agent_owned_and_idempotent`.

Combine one quiescent main, one quiescent worker, one dispatch-uncertain worker,
and one worker with a repaired interrupted tool. Resume twice, making no user
input between the first and second resumes.

Assert that every repair or warning is agent-owned and idempotent, no agent
consumes another agent's restore notice or fake lane, the second generation
adds no second repair suffix, the uncertain worker stays fail-closed without
dispatch, and current/history roster results remain stable. Do not require a
terminal outcome from the uncertain worker unless S5's separately gated
recovery question has been answered. This is the broadest headless regression
and should land only after S0-S6 localize failures well.

The checked-in fixture keeps the existing fake bounds. The main production-starts
the completed quiescent worker and held uncertain worker, while an exact durable
UI creation adds the repair-role child with its fixed lane correlation. Boot A
requires both synchronized crash cuts at once. Boot B owns the sole repair suffix
and one uncertain-worker warning. Boot C receives no input, repeats only the
worker-owned warning, and must equal Boot B in every typed durable stream and
ID-keyed roster. An explicit repair-worker prompt after those assertions consumes
the final closed continuation and proves the retained lane/error context still
belongs to that worker; the uncertain worker remains unfinished and undispatched.

### S8 — Public terminal and manual tmux acceptance

The Unix-only spawned-PTY gate is based on S1. It creates and completes the
main/worker pair in headless Boot A, where exact
`ctx_id` lane bindings are available. Boot B runs the exact universal
`tau resume <session-id>` against the same private config, stores, and checkpointed
agent-to-lane bindings. This avoids asking the public terminal's initial
no-`ctx_id` prompt to select among multiple unbound lanes.

The VT model proves only that both restored conversations can be selected,
their terminal rows do not become pending again, and a targeted worker prompt
renders after its restored transcript. Drive selection with
`:agent switch <stable-id>` using IDs from the side observer; do not add
`fzf`/picker behavior to this gate. Simultaneously require side-observer replay
boundaries, directed roster state, typed store prefixes/suffixes, exact provider
consumption, and bounded cleanup.

Finally run an opt-in warm-process manual UI smoke:

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

Configure the exact trusted provider profile first through
`~/.config/tau/testing.yaml`. Inside the isolated tmux session, create a main
and worker, inspect `:agent`, select each with `:agent switch <agent-id>`, and
send one follow-up. From a separate shell pointed at the scratch runtime, the
external roster command is `tau agent list <session-id>`.

The current helper cannot pass `tau resume SESSION` to its child Tau, and its tmux session ends
when Tau exits. Do not claim a manual cold resume through this helper. The
spawned-PTY gate above owns real-UI cold-resume acceptance. A manual tmux cold
resume remains blocked until the helper gains a tested resume/argument boundary.
Always finish the exploratory run with:

```sh
target/debug/tau dev tmux stop \
  --scratch-root "$scratch_root" \
  --session "$tmux_session" \
  --remove-scratch
rmdir -- "$scratch_parent"
```

The scratch may contain copied allowlisted authentication state; remove it
rather than retaining it as a normal failure artifact. Record only IDs,
lifecycle, selection, and completion observations. Do not assert provider
wording or make this credentialed run a CI gate.
If Tau already exited and tmux removed the session before `stop` can clean the
root, verify `$scratch_root` against the exact path printed by `start`, require
that the root and `.tau-dev-tmux-scratch` marker are non-symlinks, and compare the
regular marker's exact bytes with `tau dev tmux scratch v1\n` before removing
that exact directory manually.

## Per-scenario acceptance checklist

Every automated scenario must:

- establish its Boot A cut from exact events plus durable state;
- capture session ID and every agent ID from typed protocol data;
- compare typed pre/post membership, execution-restore, and per-agent snapshots,
  including exact record counts;
- classify every observed event as replay or live and wait for all boundaries;
- query the roster only after the session boundary;
- prove absence of unintended provider/tool execution, not merely the presence
  of expected output;
- activate restored agents by explicit stable ID, never by “current agent”
  inference;
- enforce exact scenario consumption, extension allowlists, deadlines, and
  process-group/socket cleanup;
- retain config, scenario, traces, stores, observers, stderr, and bounded PTY
  captures on failure.

Run focused targets while developing. The final implementation change must pass:

```sh
nix build -L .#ci.tests
selfci check --candidate <commit-id>
```

## Authority stop conditions

The scenarios above test current confirmed behavior. They do not authorize new
persistence, replay, provider acknowledgement, watch, navigation, or worker
ownership semantics.

Stop and request explicit user or maintainer confirmation before implementing a
test whose expected result would require any of the following:

- automatically resending a dispatch-uncertain inference;
- terminalizing or recovering a cold-restored dispatch-uncertain inference
  without a confirmed explicit operation;
- persisting or recreating watch edges or explicit navigation overrides;
- treating an `agent.started` fact as current session membership;
- cold-restoring ephemeral membership;
- claiming exactly-once backend work across a crash; or
- changing event durability, replay order, lifecycle interfaces, or recovery
  writes to make a scenario pass.

Ask the concrete question with the exact synchronized cut, competing observable
outcomes, affected stores/events, and proposed authority. Do not encode the
choice first in a fixture assertion.

## Governing records

This plan follows:

- [`SPEC-tau-harness-session-state`](../crates/tau-harness/specs/SPEC-tau-harness-session-state.md)
- [`SPEC-tau-harness-event-processing`](../crates/tau-harness/specs/SPEC-tau-harness-event-processing.md)
- [`SPEC-tau-harness-extension-lifecycle`](../crates/tau-harness/specs/SPEC-tau-harness-extension-lifecycle.md)
- [`SPEC-tau-proto-session-events`](../crates/tau-proto/specs/SPEC-tau-proto-session-events.md)
- [`SPEC-agent-watch`](../specs/SPEC-agent-watch.md)
- [`SPEC-tool-requests-and-routing`](../specs/SPEC-tool-requests-and-routing.md)
- [`SPEC-terminal-tool-reports-and-canonical-outcomes`](../specs/SPEC-terminal-tool-reports-and-canonical-outcomes.md)
- [`SPEC-provider-execution-reports-and-canonical-facts`](../specs/SPEC-provider-execution-reports-and-canonical-facts.md)
- [`SPEC-compaction-and-context-recovery`](../specs/SPEC-compaction-and-context-recovery.md)
- [`SPEC-start-agent-requests`](../specs/SPEC-start-agent-requests.md)
- [`SPEC-tau-harness-session-state`](../crates/tau-harness/specs/SPEC-tau-harness-session-state.md)
- [`SPEC-tau-proto-session-events`](../crates/tau-proto/specs/SPEC-tau-proto-session-events.md)
- [`GATE-persistence-and-extension-interface-change-approval`](../specs/GATE-persistence-and-extension-interface-change-approval.md)
- [`ARCH-tau-e2e-tests`](../crates/tau-e2e-tests/specs/ARCH-tau-e2e-tests.md)
- [`SPEC-tau-e2e-deterministic-provider`](../crates/tau-e2e-tests/specs/SPEC-tau-e2e-deterministic-provider.md)

No Linked Spec changes are required for this planning-only document because it
does not select or alter product behavior.
