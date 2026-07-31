# ARCH-tau-e2e-tests: tau-e2e-tests architecture

`tau-e2e-tests` contains two deliberately separate fixture families.

`DeterministicFixture` is always-on and hermetic. It starts an exact
Cargo-built, test-only provider subprocess through normal extension supervision,
publishes and routes `fake/test`, and normally starts only the no-side-effect
`tau-ext-test-dummy` wrapper. Gate 2 instead starts the exact universal Tau
binary as bundled `component ext-shell`. The session-restore modes install the
production harness-owned internal handlers. S1 exposes only `agent_start` to its
exact main role; S2 adds only `agent_watch`; both expose no tools to the worker.
S3 reuses the S1 surface: `UiCreateAgent`, directed roster queries, and typed
store seeding remain test-driver boundaries rather than fake-provider actions.
S4 configures one main exposing only `agent_start` and two distinct tool-free
worker roles. Its closed grammar permits at most two adjacent production starts,
retains their ordered child identities, and matches each sequential automatic
watch batch against the latest successful start.
S5 reuses S2's exact two-role tool surface and one bounded hold. Its crash oracle
correlates the same worker prompt across the durable dispatch checkpoint, decoded
fake cursor, and live `hold_ready` observation before process-group `SIGKILL`;
neither independently persisted store acknowledges the other.
S6 separately gives only the worker the supervised test dummy in exact
`hold_no_side_effect` mode; main retains only `agent_start`. Its crash oracle
requires the worker's durable request/start pair plus canonical readiness. The
strict fake observes the eager nonsemantic/durable repair order and validates the
next explicit continuation's exact balanced error context without authorizing a
tool redispatch.
S7 composes both interruption classes with a completed production worker. The
main retains the existing two-start surface for the quiescent and uncertain
children; one fixed durable `UiCreateAgent` request creates the repair child so
no fake grammar limit expands. Four checkpointed lane bindings, exact per-agent
provider budgets, typed journals, and ID-keyed rosters enforce isolation across
two no-input resumes.
Generated configuration, durable session state,
scenario data, provider trace, and extension stderr stay below a fresh private
root. The provider accepts only strict inline `ScenarioV1` or `ScenarioV2`
configuration; V2 uses bounded exact-correlation lanes and clean-resume cursor
checkpoints. It
has no network, authentication, shell, child-process, prompt-control, or
arbitrary fixture-loading capability. Tests match selected stable prompt fields
and exact V1 global-FIFO or V2 lane-local consumption rather than giant prompt
snapshots.

`VcrFixture` remains opt-in infrastructure for real provider and shell turns.
It is not sandboxed: it executes a trusted local `tau` binary, uses the user's
normal provider authentication store, and allows the shell extension to run
commands with the user's permissions. Only run it with an active VCR mode plus
`TAU_VCR_DIR` and `TAU_E2E_MODEL`. Cassettes can contain provider traffic,
prompts, tool calls, shell output, and other local test data.

The deterministic fixture covers Tau's subprocess lifecycle, CBOR protocol,
Configure/Ready gate, model publication/selection/routing, prompt construction,
provider event validation, tool dispatch/continuation, typed failures,
cancellation, fatal provider disconnect, concurrent lane isolation, clean
restore, durable projection, and headless shutdown. Its two-agent restore gate
also proves that one completed production-started durable worker remains
addressable with its own transcript and route while the daemon-lifetime
automatic watch is dropped. S2 explicitly recreates that watch after resume and
proves one fresh subscription's exact initial work-status, prompt, and response facts without treating the initial snapshot as model work. S3 composes the
same current durable pair with one valid unloaded durable history member while
proving a same-daemon ephemeral agent leaves no cold-restored transcript,
membership, route, or replay boundary. S4 restores a three-member durable
session, validates roster and replay facts by identity, activates the two
workers in reverse creation order, and rejects lane rebinding or cross-agent
transcript suffixes. Its bounded two-start checkpoint semantics remain
fixture-local and make no crash-exact coordination claim. S5 cold-restores the
interrupted worker twice, requires the mandatory dispatch-uncertain warning and
zero automatic worker submission on each boot, rejects the old watch, and allows
only a fresh Boot B watch whose initial typed status names the checkpointed
prompt. Its synchronized cut does not establish transactional checkpointing,
exactly-once external work, retry, abandonment, or recovery behavior. S6
cold-restores an acknowledged foreground dummy call, requires one
restart/possible-side-effect repair pair and no second live start, then compares
the next cold resume's membership, execution restore, and agent streams exactly
against the repaired generation. It does not claim safe retries, exactly-once
side effects, or recovery ownership for an interrupted start-agent request.
S7 requires the first resume's warning and repair to remain owned by their
distinct workers, then requires the second generation to add no provider work
or durable suffix. A post-assertion repair continuation proves only the repair
worker retained that fake lane; the uncertain worker remains fail-closed without
a terminal outcome.
It does not cover the
provider-builtin implementation, ChatGPT request lowering/parsing, WebSocket
behavior, production retries, crash-exact action replay, or broad terminal
rendering. Universal packaging is covered narrowly by Gate 1's CLI and Gate 2's
bundled core-shell component.

The original Unix-only core-resume topology is the deterministic fixture's
dummy-tool public-UI boundary.
It runs the exact built universal `tau` under a fixed real PTY, while the fake
provider and built-in test dummy remain supervised subprocesses. Boot A reaches a
durable terminal dummy result and is fully reaped; Boot B uses explicit
`tau resume <session-id>`. A bounded VT model is authoritative for the user-visible
terminal row, a replay-aware side UI peer is authoritative for delivery ordering
and replay boundaries, and typed `SessionStore`/`AgentStore` reads are
authoritative for membership and transcript prefix/suffix integrity.

The same target also owns one live dual-PTY attach baseline. The owning exact
public CLI creates the daemon, completes one closed text action, and then a
second exact public CLI explicitly attaches to that session. Normalized VT row
classes compare stable semantic elements and partial order rather than terminal
bytes. A late replay-aware socket observer proves canonical delivery order
remains unchanged while the attached UI presents current state before
transcript. Exact fake-provider consumption, bounded synchronization,
process-group teardown, and absence of runtime discovery artifacts remain
separate authorities. A correlated `HoldUntilCancel` lane also attaches only
after its hold-ready fact and requires both terminals to present the same
selected agent, then converge on its editable status after one exact
cancellation. Typed stats own the running-to-idle transition; provider traces,
stats snapshots, and side-observer prompt facts
independently exclude duplicate submission or cancellation. This baseline does
not cover shell, tool, or other local-presentation behavior. A separate
main/worker case attaches two UIs only after the complete typed roster is idle.
Both select every agent by stable public ID in opposite orders, compare
ID-keyed semantic transcript rows, and prove selection remains UI-local while
provider facts, agent runtime stats, and typed durable event streams remain unchanged.
The same dual-PTY scenario enters distinct unsubmitted drafts, clears them
independently, and applies a runtime theme in one UI. Exact visible-canary
round trips across completed PTY-read boundaries and stable-row styles prove
local redraw and theme isolation while both terminals
continue to project the same ID-keyed semantic transcript rows. Exact provider
and durable snapshots exclude submission and authoritative journal mutation.
The observer requires the expected live-only prompt-draft liveness rows; those
rows are not durable semantic state.
The narrow projection then resizes only the worker PTY from 120x40 to 80x24.
Its ordered prompt-boundary, response, idle, and selected-ID classes remain
exact. The unchanged wide peer stays on main and retains its rows, selected-ID
style, and wide-only status signature. Wrapping, spacing, adaptive field elision,
and truncation positions remain presentation-local; provider facts and durable
stores stay unchanged.

S8 adds a separate topology under that same target. A test-only headless daemon
first completes the production `agent_start` main/worker flow with exact
`ctx_id` lane bindings. After bounded process-group and socket cleanup, only
Boot B runs the exact universal `tau resume <session-id>` under the PTY against the
same private config, stores, and fake-provider checkpoint. Stable typed agent IDs
drive explicit terminal switches. The VT model is authoritative only for
transcript selection, the completed `agent_start` row remaining terminal, and
the targeted worker continuation appearing after its restored transcript.
Replay-aware socket delivery, directed rosters, typed multi-agent stores, and
exact fake-provider consumption remain independent authorities. This does not
extend terminal rendering, production-provider, crash-exact, watch-restoration,
or recovery claims.

A third core-resume topology starts the exact universal Tau under a PTY with no
agent and no tools. One private same-process sender record and callback socket
authorize an exact bare external message, which auto-starts the first agent.
Socket stats are authoritative for its `active_auto/running` state; the
correlated hold-ready notice is broadcast afterward, so its target-PTY
projection proves that UI consumed the update before one real Ctrl-J selects
the exact recipient. The provider hold is explicitly canceled and reaped after
selection. This topology does not broaden peer trust, delivery, provider,
crash, or terminal-rendering claims.

The complementary core-shell resume gate is headless so failures localize to the
production extension boundary already packaged by the same universal binary.
It replaces both daemon and `component ext-shell`, resumes the same durable
agent only after replay/context boundaries, and checks folded
`ext_core-shell_cwd`, old provider context, a byte-identical store prefix, and a
fresh relative edit. Its closed workdir/edit grammar, scratch layout, and canary
do not turn advisory core-shell permissions into a sandbox or test directory
locks.

The cancellation/liveness gate is a third, independent headless boundary. Two
durable agents bind to separate fake-provider lanes and hold exact prompt ids.
The socket observer proves each targeted cancellation has one transient
terminal, no accepted provider terminal, and no effect on the other hold. After
both bounded workers acknowledge cancellation, the most recently selected
agent reuses its immutable lane for a fresh successful prompt. Typed CBOR store
reads prove exact prompt/checkpoint membership and the absence of canceled
provider terminals; process-group and socket cleanup provide the final
quiescence boundary. This gate has no PTY, core-shell, filesystem mutation, or
cold-resume claim.
The deterministic fake provider uses the same explicit transient provider execution
report wires as production providers; tests observe harness-canonical successors.
Authority and ordering follow
[SPEC-provider-execution-reports-and-canonical-facts](../../../specs/SPEC-provider-execution-reports-and-canonical-facts.md).
