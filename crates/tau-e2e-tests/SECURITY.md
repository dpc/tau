# tau-e2e-tests security boundaries

The deterministic and VCR fixture families have different trust boundaries.

`DeterministicFixture` starts same-UID local subprocesses, which are trusted
configured extensions rather than a sandbox boundary. It ignores ambient Tau
startup override variables, checks the exact resolved extension allowlist
before spawn, uses exact canonical test binaries, and places generated config,
synthetic scenario data, session state, stderr logs, and provider trace below a
fresh private temporary root. The fake provider opens the fixed
`fake-provider.trace` filename in its fixture-owned working directory and the
fixed clean-resume cursor filename in its harness-assigned extension state
directory. It has no
network, authentication, shell, evaluation, dynamic plugin, child-process,
prompt-control, or arbitrary input-file behavior. V2 actions are selected only
by fixture-authored correlation ids and typed cancellation, never prompt
commands. Scenario counts, bytes, hold deadlines, barriers, and diagnostics are
bounded; Nix additionally runs the exact lane in a network-denied build sandbox.
Children still inherit the ordinary process environment, but the closed fake
does not read provider credentials or use environment values as control.
The closed fixture grammar reserves exact canonical fieldless
`<user>...</user>` syntax for HumanUi provider projections because provider text
does not itself carry durable provenance. Under its closed fixture convention,
the fake projects fixture-authored expected HumanUi text and compares provider
bytes without decoding. Raw `</user>` and raw `&lt;/user&gt;` intentionally
collide in the one-way provider form; the fixture never invents one semantic
value from that form. This is test matching, not prompt-control authority, and a future non-Human
fixture needing literal canonical wrapper syntax requires a typed scenario
distinction.

The S1 session-restore fixture enables the production harness-owned `agent_start`
built-in only for its closed main role; S2 adds only `agent_watch`. The worker
role has no tools. S3 reuses the S1 tool surface. Its promptless ephemeral worker
is created only through the normal `UiCreateAgent` protocol. The seeded unloaded
worker uses fixed synthetic identity and metadata and is appended only after
Boot A releases both typed stores; reopening `AgentStore` and `SessionStore`
validates the sequence-zero creation and adjacent durable load/unload before
Boot B starts. This seeding is fixture-owned persistence setup, not a fake-provider
file-write or a public unload operation. S4 enables the same sole `agent_start`
tool for its main and configures two distinct tool-free worker roles. S5 reuses
the S2 main/worker surface for one synchronized interrupted-worker restore. S6
adds only the exact `restart_test_dummy` tool to its worker and fixes that
extension to `hold_no_side_effect`; the main retains only `agent_start`.
S7 keeps that dummy mode and one main exposing only `agent_start`. Its
production starts are limited to the existing quiescent and uncertain child
pairs. The test driver adds the repair-role durable child through one exact
`UiCreateAgent` request with fixed parent, role, prompt, and `ctx_id`; only that
role exposes `restart_test_dummy`. This does not add a fake action or raise the
existing lane, per-lane action, production-start, or scenario-byte limits.

Each S1/S2/S3 production-worker start sequence uses one adjacent
`AgentStartCall`/`AgentStartResult` pair. It requires the exact production schema
and fixture-authored arguments, then records only the distinct self/child
identities minted by the harness. S4 permits exactly two such sequential pairs
and checkpoints both child identities with contiguous start ordinals; later
automatic-watch matching targets only the latest successful start. Explicit
`agent_watch` remains restricted to the one-child S2 fixture. Because a
production-started worker has no
initial `ctx_id`, its first prompt may bind only the unique unconsumed lane with
identical configured text; zero or multiple candidates fail closed. That
immutable binding and the parent/child associations are checkpointed with the
scenario cursors.
The closed S2 watch action derives its target only from that association, always
enables, and requires an exact successful sanitized result without exposing
subscription identity.

Automatic-watch actions admit only live, model-visible records for the current
closed action with the exact retained child sender, parent recipient, bounded
configured content or typed runtime state, and stable subscription/generation
correlation. Each accepted record enters the bounded ordered live queue. One
provider prompt consumes
the complete already-delivered prefix for the current closed action, so multiple
accepted records may coalesce into one prompt; there is no general scheduler or
arbitrary message-routing control. Unrelated or excess records fail admission, and
replay cannot release an action. This fixture proves a clean, quiescent S1 resume of
one completed worker and non-persistence of its daemon-lifetime watch. S4 proves the
bounded counterpart for two sequentially started workers, including retained lane
and journal ownership under
reverse-creation activation. S2 proves explicit recreation creates one new subscription
and admits only its exact bounded live notifications; the initial snapshot
cannot activate the provider. S5 proves only fail-closed dispatch-uncertain
restoration after a synchronized interrupted worker dispatch. It does not prove
watch persistence, exactly-once external work, crash-exact provider/harness
checkpoint coordination, or any retry, abandonment, or recovery operation.

The S6 dummy hold is closed no-side-effect fixture code. It accepts no arguments
or runtime control, performs no filesystem, network, environment, or
child-process operation, and allows only one active invocation. A one-second
worker-start bound gates one exact correlated `tool.progress` readiness fact; a
ten-second deadline terminalizes a surviving invocation. Exact cancellation
wakes and joins the worker, while protocol disconnect and extension teardown join
it without fabricating a tool terminal. S6 kills only after the worker's durable
request/start pair and canonical readiness, removes the dead generation's
fixture-owned socket after proving the process group exited, and probes the
session lock before resume. Its repair-aware provider grammar accepts only the
fixed call ID, error status, and full restart/possible-side-effect diagnostic.
S7 combines that cut with S5's independently synchronized provider hold. Its
decoded checkpoint must contain exactly four immutable lane bindings and two
contiguous main-owned production child associations. Boot B and the no-input
portion of Boot C admit no provider prompt; only the uncertain worker is named
by the per-generation restore warning, and only the repair worker may receive
the sole durable repair suffix. Boot C must equal Boot B before a final explicit
repair continuation.
No retry, abandonment, or terminal outcome is requested for the uncertain worker.

Mismatch, startup, or exact-consumption errors retain the private root and print
its path. Successful roots are deleted unless `TAU_E2E_KEEP_ARTIFACTS=1`.
Artifacts contain only synthetic fixture data but remain private by default.
The harness owns supervised extension termination; a killable test-only child
wrapper owns daemon process cleanup on early test failure. Successful daemon
finish requires the entire process group to disappear without a signal; forced
TERM/KILL containment is reported as test failure. Daemon tests cover
typed failure, cancellation/timeout, same-agent post-cancel liveness,
concurrency, fatal disconnect, clean restore, and explicit S5/S6/S7 `SIGKILL`
cuts. The direct ungraceful-kill path requires the complete private process
group to disappear under a hard deadline but deliberately does not require
graceful socket cleanup or a successful parent exit. The cancellation gate accepts
only harness-minted prompt ids already held by its two bounded lanes; it cannot
cancel arbitrary sessions or agents. Its same-agent continuation proves
warm-process ordinary-inference liveness and late-terminal rejection, not
crash-exact cancellation persistence or abandonment of standalone-compaction
ownership. Provider cursor and harness journal writes are not transactional, so
crash-exact replay is explicitly outside this boundary. The fixture does not
claim broad terminal rendering or universal packaging.

S5 observes the existing bounded hold's prompt-correlated `hold_ready` trace
only after its wait worker starts. The test correlates that live record with the
same durable harness dispatch prompt and a separately decoded fake lane cursor,
then kills the process group before the hold deadline. This three-way
synchronization narrows the interruption cut; the readiness trace is not a
provider acknowledgement, and the independently persisted cursor and harness
journal remain non-transactional.

The Unix-only core-resume gate is narrower and stronger at the public UI
boundary. It launches the exact built universal Tau executable with `env_clear`,
private owner-only HOME/XDG roots, a fixed working directory and PTY, and only
the fake provider plus the built-in no-side-effect test dummy. The fixture owns
the child process session/process group, verifies it disappears after bounded
TERM-to-KILL escalation, bounds its reader shutdown, and verifies runtime socket
metadata and the durable session lock are released before Boot B. Captured PTY bytes and frames are bounded; generated configuration is
synthetic and retained only on failure or explicit opt-in. This establishes one
quiescent completed-tool cold-resume projection, not arbitrary terminal fidelity,
crash consistency, filesystem sandboxing, or safe execution of other tools.
Its live-attach case concurrently owns a second exact public CLI and fixed PTY
against the first CLI's daemon. The second process receives only the same private
HOME/XDG inputs and explicit session identity; it cannot reconfigure the daemon.
Both process groups and bounded PTY readers are independently reaped before the
fixture checks that runtime discovery artifacts and the session lock disappeared.
The closed surface permits one fake-provider text action for text-only attach or
two fake-provider actions around one `restart_test_dummy` invocation for tool
attach. Completed-tool attach uses deterministic success. Pending-tool attach
uses the fixture-private authenticated release socket only after both PTYs show
the same pending row. Attachment itself consumes no provider action and invokes
no tool. The separate interruption case retains its bounded
hold-until-cancel action. These cases prove parity snapshots plus typed terminal
facts for selected-agent, pending, and settled states; they do not prove absence
of intermediate redraw flicker, arbitrary multi-client ordering, production
provider behavior, or concurrent prompt/tool safety.
The main/worker presentation case attaches only after both typed agents are idle,
then limits both public UIs to draft editing/clearing, stable-ID selection, and
one runtime theme command. Exact visible-canary round trips across completed
PTY-read boundaries and stable-row styles guard local draft, selection, theme,
and redraw state. Exact provider traces,
observer provider/stats facts, and typed durable snapshots independently prove
that these actions submit no model work, mutate no agent runtime stats, and
append no authoritative semantic event. The observer does require the expected
live-only prompt-draft liveness rows, which are not durable semantic state.
One PTY alone is resized from 120x40 to 80x24; ordered transcript classes, one
selected stable ID and its cell style, and the separate idle class remain
authoritative while wrapping, adaptive field elision, spacing, and truncation
positions may differ.
An external uncatchable kill of the test process itself prevents Rust `Drop`
cleanup; the mandatory Nix/nextest runner remains the outer process/sandbox owner
for that residual case.

S8's companion core-resume case adds a test-only headless Boot A process before
the universal PTY resume. It enables only the synthetic fake provider, exposes
only harness-owned `agent_start` to the fixed main role, and exposes no tools to
the fixed worker role. Prompts, roles, provider output, and lane bindings are
closed scenario data; private `env_clear` HOME/XDG roots provide the only config,
state, runtime, and checkpoint inputs. The fixture owns the daemon/provider
process group and generation socket through bounded TERM/KILL cleanup, retains
only bounded stderr/observer/PTY diagnostics, and verifies the socket, lock, and
process group disappear before reuse. This adds no network, credential, shell,
arbitrary prompt, production-provider, or manual cold-resume authority.

The peer-navigation PTY case exposes no tools and starts with no agent. Its
fixture-owned sender record and callback socket live only under the private
runtime root, authorize one exact typed request, and add no general network or
credential authority. A bounded fake-provider hold preserves the receiver's
live interval long enough to exercise the real Ctrl-J binding, then an exact
prompt cancellation reaps it. The case proves navigation eligibility, not
stronger peer authentication or delivery semantics.
Callback accept/read/write run synchronously under one absolute deadline with no
detached worker; negative fixtures cover both an absent callback and a complete
Hello that stalls before authentication.

The headless core-shell resume gate is a controlled production-extension
exception. It enables only the fake provider and exact universal
`component ext-shell`, exposes only `workdir` and `edit`, keeps directory locking
disabled, and supplies a closed grammar with fixed relative paths and no command
interpreter. Core-shell still runs with same-UID filesystem permissions and is
not a sandbox. A private scratch tree, symlink rejection, exact target bytes,
wrong-path absence, and an outside canary bound the scenario and detect drift;
the Nix build sandbox remains the outer isolation boundary.

`VcrFixture` is deliberately non-hermetic. It can use real provider credentials
and lets `core-shell` execute with the user's permissions. Its cassettes can
contain prompts, provider traffic, tool calls, output, and local paths. Run and
share them only under the policy in the crate README.

Re-review this boundary when adding scenario actions, subprocesses, environment
inputs, filesystem reads, network access, live control, concurrency, new tools,
or broader artifact retention.
