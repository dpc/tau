# ARCH-tau-e2e-tests: tau-e2e-tests architecture

`tau-e2e-tests` contains two deliberately separate fixture families.

`DeterministicFixture` is always-on and hermetic. It starts an exact
Cargo-built, test-only provider subprocess through normal extension supervision,
publishes and routes `fake/test`, and optionally starts only the no-side-effect
`tau-ext-test-dummy` wrapper. Generated configuration, durable session state,
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
restore, durable projection, and headless shutdown. It does not cover the
provider-builtin implementation, ChatGPT request lowering/parsing, WebSocket
behavior, production retries, crash-exact action replay, universal-binary
packaging, or broad terminal rendering.

The Unix-only core-resume gate is the deterministic fixture's public-UI boundary.
It runs the exact built universal `tau` under a fixed real PTY, while the fake
provider and built-in test dummy remain supervised subprocesses. Boot A reaches a
durable terminal dummy result and is fully reaped; Boot B uses explicit
`tau -r <session-id>`. A bounded VT model is authoritative for the user-visible
terminal row, a replay-aware side UI peer is authoritative for delivery ordering
and replay boundaries, and typed `SessionStore`/`AgentStore` reads are
authoritative for membership and transcript prefix/suffix integrity.
