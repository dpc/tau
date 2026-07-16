# ARCH-tau-e2e-tests: tau-e2e-tests architecture

`tau-e2e-tests` contains two deliberately separate fixture families.

`DeterministicFixture` is always-on and hermetic. It starts an exact
Cargo-built, test-only provider subprocess through normal extension supervision,
publishes and routes `fake/test`, and optionally starts only the no-side-effect
`tau-ext-test-dummy` wrapper. Generated configuration, durable session state,
scenario data, provider trace, and extension stderr stay below a fresh private
root. The provider accepts only strict inline `ScenarioV1` configuration; it
has no network, authentication, shell, child-process, prompt-control, or
arbitrary fixture-loading capability. Tests match selected stable prompt fields
and exact FIFO scenario consumption rather than giant prompt snapshots.

`VcrFixture` remains opt-in infrastructure for real provider and shell turns.
It is not sandboxed: it executes a trusted local `tau` binary, uses the user's
normal provider authentication store, and allows the shell extension to run
commands with the user's permissions. Only run it with an active VCR mode plus
`TAU_VCR_DIR` and `TAU_E2E_MODEL`. Cassettes can contain provider traffic,
prompts, tool calls, shell output, and other local test data.

The deterministic fixture covers Tau's subprocess lifecycle, CBOR protocol,
Configure/Ready gate, model publication/selection/routing, prompt construction,
provider event validation, tool dispatch/continuation, durable projection, and
headless shutdown. It does not cover the provider-builtin implementation,
ChatGPT request lowering/parsing, WebSocket behavior, production retries,
universal-binary packaging, or terminal rendering.
