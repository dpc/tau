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
It disables every unrelated built-in extension and normally enables only the
no-side-effect `tau-ext-test-dummy` success mode. Gate 2 is the sole controlled
exception: the exact universal `component ext-shell` may expose only `workdir`
and `edit` to a closed scratch-only scenario. The fake has no network,
authentication, shell, evaluation, child-spawn, prompt-control, environment
control, or arbitrary fixture-file behavior.

Hermetic embedded and daemon launches bypass ambient Tau startup-role,
role/config, extension, and secret environment transports, retain that policy
for runtime settings reloads, then check an exact extension-name allowlist before
any process starts. This is a deterministic-test exception to
normal interactive startup availability in
[DESIGN-extension-availability-startup](../../../specs/DESIGN-extension-availability-startup.md);
embedded launches do not scrub the ordinary child OS environment. Spawned daemon
acceptance clears it and supplies private HOME/XDG roots plus a fixed locale.

`ScenarioV1` remains the closed phase-one grammar. `ScenarioV2` adds at most
eight exact `ctx_id` lanes with independent bounded cursors. It supports typed
terminal errors, exact cancellation holds with hard timeouts, deliberate
disconnect, and named barriers whose participants must all submit before any
lane completes. It also has one narrow adjacent action pair for the allowlisted
`restart_test_dummy` empty-argument call and exact successful result; arbitrary
tool names, arguments, and results remain outside the grammar. A barrier is the lane's sole action, appears once per distinct
participant lane, and has one consistent bounded participant count, preventing
same-lane and cyclic barrier plans. Initial `ctx_id` binds an agent to one lane.
The public terminal UI supplies no initial `ctx_id`, so an unbound first prompt
may select the sole configured lane; multi-lane scenarios still require an exact
`ctx_id`. Continuations cannot change that binding. The fake subscribes only to live prompt/cancel
traffic, so restored event replay cannot consume actions.

The core-shell action family is likewise closed rather than generic: it can set
only relative `project`, edit only `resume-sentinel.txt` with the two fixed
line-range shapes, and vary only bounded call IDs, nonce text, prompts, and final
markers. Its result continuations require success; the resumed provider prompt
must contain both the old nonce-bearing transcript and restored workdir context.

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
projection, clean restore/shutdown, and the spawned public terminal's completed
tool projection across one quiescent cold resume. Sequential error then success is two
explicit user turns, not provider retry evidence. It is not evidence for
provider-builtin, upstream request/parsing, ChatGPT/WebSocket fidelity,
production retry scheduling, crash-exact replay, universal packaging beyond
the exact Gate 1 CLI and Gate 2 bundled core-shell components, or
broad terminal rendering fidelity. Live/VCR and transcript-replay fixtures remain separate.

Refines [ARCH-tau-e2e-tests](ARCH-tau-e2e-tests.md).
