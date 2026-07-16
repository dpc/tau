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

Mismatch, startup, or exact-consumption errors retain the private root and print
its path. Successful roots are deleted unless `TAU_E2E_KEEP_ARTIFACTS=1`.
Artifacts contain only synthetic fixture data but remain private by default.
The harness owns supervised extension termination; a killable test-only child
wrapper owns daemon process cleanup on early test failure. Daemon tests cover
typed failure, cancellation/timeout, concurrency, fatal disconnect, and clean
restore. Provider cursor and harness journal writes are not transactional, so
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

`VcrFixture` is deliberately non-hermetic. It can use real provider credentials
and lets `core-shell` execute with the user's permissions. Its cassettes can
contain prompts, provider traffic, tool calls, output, and local paths. Run and
share them only under the policy in the crate README.

Re-review this boundary when adding scenario actions, subprocesses, environment
inputs, filesystem reads, network access, live control, concurrency, new tools,
or broader artifact retention.
