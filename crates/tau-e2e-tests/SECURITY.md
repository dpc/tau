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

The session-restore fixture enables the production harness-owned `agent_start`
built-in only for its closed main role; the worker role has no tools. Its one
bounded start action requires the exact production schema and fixture-authored
arguments, then records only the distinct self/child identities minted by the
harness. Because a production-started worker has no initial `ctx_id`, its first
prompt may bind only the unique unconsumed lane with identical configured text;
zero or multiple candidates fail closed. That immutable binding and the
parent/child association are checkpointed with the scenario cursors.

Automatic-watch actions admit only live, model-visible records for the current
closed action with the exact retained child sender, parent recipient, bounded
configured content or typed runtime state, and stable subscription/generation
correlation. Unrelated and excess records fail before queue admission. Each
accepted live record releases one provider prompt; there is no general scheduler
or arbitrary message-routing control. Replay cannot release an action. This fixture proves a
clean, quiescent resume of one completed worker and non-persistence of its
daemon-lifetime watch. It does not prove interrupted-request recovery, watch
persistence, or crash-exact provider/harness checkpoint coordination.

Mismatch, startup, or exact-consumption errors retain the private root and print
its path. Successful roots are deleted unless `TAU_E2E_KEEP_ARTIFACTS=1`.
Artifacts contain only synthetic fixture data but remain private by default.
The harness owns supervised extension termination; a killable test-only child
wrapper owns daemon process cleanup on early test failure. Successful daemon
finish requires the entire process group to disappear without a signal; forced
TERM/KILL containment is reported as test failure. Daemon tests cover
typed failure, cancellation/timeout, same-agent post-cancel liveness,
concurrency, fatal disconnect, and clean restore. The cancellation gate accepts
only harness-minted prompt ids already held by its two bounded lanes; it cannot
cancel arbitrary sessions or agents. Its same-agent continuation proves
warm-process ordinary-inference liveness and late-terminal rejection, not
crash-exact cancellation persistence or abandonment of standalone-compaction
ownership. Provider cursor and harness journal writes are not transactional, so
crash-exact replay is explicitly outside this boundary. The fixture does not
claim broad terminal rendering or universal packaging.

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
An external uncatchable kill of the test process itself prevents Rust `Drop`
cleanup; the mandatory Nix/nextest runner remains the outer process/sandbox owner
for that residual case.

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
