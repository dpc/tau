# DESIGN-tau-e2e-deterministic-provider: Deterministic provider boundary

Status: confirmed, 2026-07-15, dpc

Deterministic harness acceptance uses a test-only supervised provider subprocess
inside `tau-e2e-tests`. It is launched by exact path as a required custom
provider and is never added to the universal binary, built-in registry, normal
provider discovery, or provider self-knowledge.

Control is a strict versioned `ScenarioV1` supplied in startup Configure data.
Turns match selected stable typed prompt projections in exact FIFO order and
emit ordinary provider events. Dynamic prompt identities are copied from the
request; provider-authored tool call IDs must return unchanged in tool-result
continuations. Unknown configuration, unexpected prompts, overlaps, first
mismatches, and unconsumed turns fail closed with bounded synthetic diagnostics.

The fixture uses fresh private config, state, session, and artifact directories.
It disables every unrelated built-in extension and may enable only the
no-side-effect `tau-ext-test-dummy` success mode. The fake has no network,
authentication, shell, evaluation, child-spawn, prompt-control, environment
control, or arbitrary fixture-file behavior.

The embedded launch explicitly bypasses ambient Tau startup-role, role/config,
and extension environment transports, then checks an exact extension-name
allowlist before any process starts. This is a deterministic-test exception to
normal interactive startup availability in
[DESIGN-extension-availability-startup](../../../specs/DESIGN-extension-availability-startup.md);
it does not scrub the ordinary child OS environment.

Phase one accepts only one text turn or one matching tool-call/tool-result pair.
`DeterministicFixture::run_turn` is the authority that requires exact scenario
consumption before returning success.

This boundary validates extension supervision, CBOR lifecycle, model routing,
prompt assembly, provider-event validation, one real tool continuation, durable
session projection, and clean headless shutdown. It is not evidence for
provider-builtin, upstream request/parsing, WebSocket pooling/recovery/timeouts,
production retries, universal packaging, or terminal rendering. Live/VCR and
transcript-replay fixtures remain separate.

Refines [ARCH-tau-e2e-tests](ARCH-tau-e2e-tests.md).
