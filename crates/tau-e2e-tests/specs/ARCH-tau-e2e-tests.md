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
proves one fresh subscription's exact initial, prompt, running, response, and
idle facts without treating the initial snapshot as model work. S3 composes the
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
exactly-once external work, retry, abandonment, or recovery behavior. It does not cover the
provider-builtin implementation, ChatGPT request lowering/parsing, WebSocket
behavior, production retries, crash-exact action replay, or broad terminal
rendering. Universal packaging is covered narrowly by Gate 1's CLI and Gate 2's
bundled core-shell component.

The Unix-only core-resume gate is the deterministic fixture's public-UI boundary.
It runs the exact built universal `tau` under a fixed real PTY, while the fake
provider and built-in test dummy remain supervised subprocesses. Boot A reaches a
durable terminal dummy result and is fully reaped; Boot B uses explicit
`tau -r <session-id>`. A bounded VT model is authoritative for the user-visible
terminal row, a replay-aware side UI peer is authoritative for delivery ordering
and replay boundaries, and typed `SessionStore`/`AgentStore` reads are
authoritative for membership and transcript prefix/suffix integrity.

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
