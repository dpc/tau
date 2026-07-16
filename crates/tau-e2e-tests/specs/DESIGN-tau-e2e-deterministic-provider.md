# DESIGN-tau-e2e-deterministic-provider: Deterministic provider boundary

Status: confirmed, 2026-07-16, dpc

Deterministic harness acceptance uses a test-only supervised provider subprocess
inside `tau-e2e-tests`. It is launched by exact path as a required custom
provider and is never added to the universal binary, built-in registry, normal
provider discovery, or provider self-knowledge.

Control is strict versioned `ScenarioV1` or `ScenarioV2` data supplied in startup
Configure. V1 uses one global FIFO; V2 uses independent lane-local FIFOs. Actions
match selected stable typed prompt projections and emit ordinary provider
events. Dynamic prompt identities are copied from the request; provider-authored
tool call IDs must return unchanged in tool-result continuations. Unknown
configuration, unexpected prompts, overlaps, first mismatches, and unconsumed
actions fail closed with bounded synthetic diagnostics.

The fixture uses fresh private config, state, session, and artifact directories.
It disables every unrelated built-in extension and may enable only the
no-side-effect `tau-ext-test-dummy` success mode. The fake has no network,
authentication, shell, evaluation, child-spawn, prompt-control, environment
control, or arbitrary fixture-file behavior.

Hermetic embedded and daemon launches bypass ambient Tau startup-role,
role/config, extension, and secret environment transports, retain that policy
for runtime settings reloads, then check an exact extension-name allowlist before
any process starts. This is a deterministic-test exception to
normal interactive startup availability in
[DESIGN-extension-availability-startup](../../../specs/DESIGN-extension-availability-startup.md);
it does not scrub the ordinary child OS environment.

`ScenarioV1` remains the closed phase-one grammar. `ScenarioV2` adds at most
eight exact `ctx_id` lanes with independent bounded cursors. It supports typed
terminal errors, exact cancellation holds with hard timeouts, deliberate
disconnect, and named barriers whose participants must all submit before any
lane completes. A barrier is the lane's sole action, appears once per distinct
participant lane, and has one consistent bounded participant count, preventing
same-lane and cyclic barrier plans. Initial `ctx_id` binds an agent to one lane; continuations
cannot change that binding. The fake subscribes only to live prompt/cancel
traffic, so restored event replay cannot consume actions.

V2 cursors and immutable agent-to-lane bindings are atomically checkpointed in
the harness-assigned extension state directory and restored only after validating
the complete scenario identity, binding uniqueness, and bounds. This supports
clean, quiescent daemon stop/resume with no in-flight action. The checkpoint and
harness journal are not transactionally committed together, so this fixture
makes no crash-exact action-replay claim; such a claim requires a provider
acknowledgement protocol design.

Daemon acceptance uses the normal local socket protocol and real supervised
subprocess. Its `ServeOptions` explicitly bypass ambient startup override
transports and checks the same exact extension allowlist before spawning as the
embedded fixture. Normal daemon defaults are unchanged.

This boundary validates extension supervision, CBOR lifecycle, model routing,
prompt assembly, provider-event validation, one real tool continuation, typed
terminal error projection, exact cancellation, bounded provider stalls, fatal
provider-disconnect handling without restart, lane isolation, durable session
projection, and clean restore/shutdown. Sequential error then success is two
explicit user turns, not provider retry evidence. It is not evidence for
provider-builtin, upstream request/parsing, ChatGPT/WebSocket fidelity,
production retry scheduling, crash-exact replay, universal packaging, or
terminal rendering. Live/VCR and transcript-replay fixtures remain separate.

Refines [ARCH-tau-e2e-tests](ARCH-tau-e2e-tests.md).
