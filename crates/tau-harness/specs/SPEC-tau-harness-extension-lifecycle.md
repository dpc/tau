# SPEC-tau-harness-extension-lifecycle: Extension Lifecycle

Architectural or externally meaningful functional changes to this
harness-extension contract are subject to
[GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

## Record justification

The extension lifecycle contract spans configuration, process supervision,
protocol routing, declaration staging, collision checks, and harness startup,
so no single implementation artifact can own it coherently.

## Daemon listener and accept forwarding

Daemon IPC sockets are bound or socket-activated before the harness event loop
starts. A small accept-forwarder thread converts accepted Unix streams into
`HarnessEvent::NewClient` messages for the event loop; all client protocol
validation still happens after the stream reaches the harness.

The forwarder waits reactively on the listener fd plus an owned wake fd. Dropping
the forwarder wakes and joins the thread before the daemon listener handle is
dropped, preserving socket cleanup ownership while avoiding sleep polling and
path-based shutdown races.

## Extension boundary

Memory-only harnesses preserve the same Hello, Configure, declaration, Ready,
collision, and required/optional failure lifecycle. Configure carries
`state_dir = None`, and the harness delegates no Session, User, or Cache
extension-data storage. Extensions remain trusted same-user executables and
unsandboxed; direct filesystem, network, or external-service side effects are
outside the harness-managed storage contract.

One supervised extension instance publishes at most one `extension.exited`
lifecycle fact. A disconnect handled before harness shutdown owns that fact;
later shutdown joins/cleans the child without publishing a duplicate. A still
connected child publishes its one exit fact during orderly shutdown.

## Overall harness shutdown

Overall harness shutdown first disconnects every extension transport and closes
component ingress. In-process provider and tool runners normally observe that
transport closure or EOF and return. The harness gives every in-process runner
one shared finite cleanup grace beginning at transport closure, then joins only
runners that have actually terminated. It transfers each runner handle to a
detached background join-reaper, which reports a completed join without letting
the runner's final Rust teardown block harness shutdown. A runner still alive
when the shared deadline expires emits a warning and the harness drops its
reaper result channel; the detached reaper retains the runner handle until it
can join. Neither thread is force-cancelled because Rust provides no safe
per-thread equivalent of process signals.

A detached runner can continue work and retain arbitrary resources or side
effects until host-process exit. Normal Tau process exit lets the OS reclaim
those resources, but an embedded or reusable host can accumulate them across
shutdown/restart cycles. Process-backed extensions keep their existing
supervised process signal-and-reap policy. This shutdown-only fault containment
does not change normal disconnect handling or make configured local extensions
a hostile-process containment boundary.

Extensions are less-trusted peers connected over the Tau protocol. They may
publish ordinary events through `emit`, subscribe to committed events, register
interceptors, provide tools/actions/context, and request extension-data file
operations. The harness validates source ownership for harness-owned or
provider-owned facts and rejects peer-authored lifecycle, membership,
transcript, prompt, and harness-status facts unless they arrive through the
specific API path that owns them. Interceptor replacement is intentionally
conservative: protected facts may be observed, but drops and forbidden rewrites
publish the original event so routing identities and durable folds stay aligned.
Mutable prompt-text events may be rewritten only without changing their routing
identity. The `internal_kind` tag is part of that identity, and tagged
context-size alert and background-tool completion text is also immutable so the
committed fact retains the exact harness-authored advisory or lifecycle notice.

“Less-trusted” here concerns protocol authority and integrity: extensions cannot
author harness/provider-owned facts or bypass ownership and lifecycle validation.
Configured extension processes remain trusted local executables, and their local
stdio IPC is not an adversarial availability or sandbox boundary. Per-operation
robustness limits remain useful, but generic hostile-frame, slowloris, and
connection-flood hardening is outside this lifecycle contract unless an approved
threat-model change says otherwise. See
[`SECURITY.md`](../../../SECURITY.md) and
[`SPEC-tau-harness-session-state`](SPEC-tau-harness-session-state.md#extension-data).

The harness also tracks loaded session membership in runtime state before the
corresponding must-pass `session.agent_loaded` publish commits. That keeps
idempotency stable while an interceptor parks publication and prevents duplicate
membership/start facts from being queued for the same live agent.

Provider tool calls are evaluated against the tool snapshot owned by the prompt
that produced them. Model-visible rejection diagnostics for those calls must use
that same snapshot for availability wording and near-name suggestions; current
role/model policy is only the authority when no prompt-owned snapshot exists.
Tool examples are registration metadata, not prompt-surface definitions: rendered
tool definitions omit them, and the harness surfaces at most one bounded relevant
example after a failed call in an agent branch.

Extensions that need to turn an internal wakeup into an agent prompt use
`extension.internal_prompt_submit_request`. The harness accepts this narrow
request only from authenticated configured extension entries. It crosses
ordinary interception and commit before the harness validates the target loaded agent and
submits internal model-classified text through the ordinary prompt queue. It
has no user-message class and does not update user-interaction metadata, but
still wakes queued agents. The durable transcript fact remains the
harness-owned internal `agent.prompt_submitted` or `agent.prompt_steered`,
stamped with the authenticated configured extension name. External user
messages use immutable `message.delivered` facts; extensions may not forge
prompt transcript facts.
Pre-Ready requests are operational traffic ordered behind activation. See
[SPEC-internal-prompt-submit-requests](../../../specs/SPEC-internal-prompt-submit-requests.md).

`agent.start_request` is also globally ordered operational traffic rather than
an activation declaration. Its complete raw Emit remains in the bounded
deferred-message queue until Ready and global activation, then crosses ordinary
interception and commit before any acceptance, rejection, rebinding, or side-agent
work. See
[SPEC-start-agent-requests](../../../specs/SPEC-start-agent-requests.md).

Terminal-output events are the same globally ordered operational traffic rather
than activation declarations. Pre-Ready bells and OSC requests remain deferred
until activation, then enter ordinary publication in original arrival order. See
[SPEC-terminal-output-side-effect-events](../../../specs/SPEC-terminal-output-side-effect-events.md).

Custom extension-owned emits are also globally ordered operational traffic.
Their complete pre-Ready frames remain deferred until activation, and disconnect
drops unreleased frames. See
[SPEC-custom-extension-events](../../../specs/SPEC-custom-extension-events.md).

Configured extensions of every configured kind request user-visible diagnostics
with `extension_notice_request(message, level)`. The request is operational
traffic: pre-Ready requests consume the existing activation message and encoded
byte quotas, retain global input order, release after Ready and the global
barrier, and disappear on disconnect before release. Unconfigured or disconnected
origins are silently denied; an illegal-phase request follows the normal protocol
failure path. The handler caps `critical` to `warning` and creates a harness-sourced,
live-only `extension.notice` with `always_show = false`. The result then crosses
ordinary interception and broadcasts to every current matching subscriber. A
publisher disconnect after inline handling does not cancel a parked
harness-authored output. Generic extension `Emit(harness.notice)` remains denied,
and `ConfigError` retains its separate mandatory replayable diagnostic path. See
[SPEC-extension-notice-requests](../../../specs/SPEC-extension-notice-requests.md).

Cross-harness agent messages use the dedicated `ExternalAgentMessage` protocol
RPC, not `Emit`. The sender-side built-in `message` tool parses bare
`&<session-id>` plus the exact-agent forms `&<session-id>/@<agent-id>` and
`<session-id>/<agent-id>`, treats the current session as local, mints a
per-message bearer capability bound to sender identity, recipient, message body,
and message/watch-response kind, and performs runtime-dir lookup plus socket
round-trip on a helper thread. Completion returns to the event loop as a
`HarnessCommand`, so target socket latency never blocks normal event processing.
The receiver accepts the RPC only from a socket peer that completed the narrow
external-message hello, validates bounded syntax and its active
`current_session_id`, then calls back to the claimed sender harness from a helper
thread to correlate the capability and bound fields. Recipient and bare-policy
validation occurs on the event loop before publishing the harness-owned inbound
`agent.message_received` projection. A bounded in-memory continuation sends
success only after that exact projection commits. Generic peer-authored
`agent.message_sent`/`agent.message_received` emits remain rejected.
Bare route capabilities are authenticated before the target selects one
configured inter-session receiver; exact capabilities preserve known-address
delivery to agents without receiver capability. Authenticated peer bodies remain
escaped agent content in a typed peer-message prompt envelope, never harness
instructions.
Runtime-dir discovery verifies matching candidates by connecting to their
sockets. Discovery never unlinks runtime files because a failed probe, metadata
PID liveness check, and pathname identity check cannot atomically exclude PID
reuse or a replacement listener. Owned CLI shutdown closes the initial-client
transport first so the daemon's exit-on-disconnect cleanup normally removes its
own lifecycle pair; forced termination remains a bounded fallback.

## Extension configuration errors

After `Hello`, the harness sends `Configure` before accepting declarations and
`Ready`. Its optional tool prefix is retained with the configured instance
across respawn. Registrations from a prefixed instance must place internal
names, visible aliases, and groups inside the assigned exact component envelope.
Final internal-name ownership is unique across live connections; prompt
snapshots separately reject simultaneously visible alias collisions. These
rules implement
[SPEC-extension-tool-prefixes](../../../specs/SPEC-extension-tool-prefixes.md).
The client runtime applies structural mapping through its logical builder
declarations, scoped factories, and dynamic registration helpers. Raw `emit`
remains a wire-level operation and receives no rewriting.

A same-connection refresh may replace its owned registration, while a
cross-connection registration of an owned final internal name is rejected.
Exact tool and group policy addresses final names; semantic tag policy continues
to span instances. Dispatch, completion provenance, replay, persisted history,
and UI retain the final names already carried by protocol facts.
Sending `Ready` records readiness but does not publish `extension.ready` or expose
staged capabilities until every initial extension has either sent `Ready` or
become terminal. The harness then resolves all final-name collisions in one
deterministic preflight, disconnects optional losers without advancing prompts,
activates every survivor as one barrier, marks and publishes all lifecycle
readiness, replays globally ordered operational traffic received behind the
barrier, and only then permits prompt/session advancement.
Only the exhaustive set of capability declarations enters activation staging;
all other emitted events are classified as operational by default. This keeps
new state-mutating, reply, progress, and terminal events behind the barrier
unless the protocol explicitly promotes them to declarations.
Pre-Ready prompt-fragment declarations reserve count and encoded bytes before
interception, block Ready until pass/drop/disconnect settles, and stage only the
committed survivor. See
[SPEC-prompt-fragment-declarations-and-projection](../../../specs/SPEC-prompt-fragment-declarations-and-projection.md).
Session-provider registration and complete session discovery snapshots use the
same pre-interception activation reservation boundary; readiness remains ordered
operational traffic. Per-agent registration, complete correlated discovery
snapshots, and keyed context use the same bounded boundary. See
[SPEC-session-discovery-declarations-and-readiness](../../../specs/SPEC-session-discovery-declarations-and-readiness.md).
Per-agent readiness is operational traffic and therefore remains ordered behind activation. See
[SPEC-per-agent-context-declarations-and-readiness](../../../specs/SPEC-per-agent-context-declarations-and-readiness.md).
Harness-internal tool handlers are installed before this preflight, so their names
participate as reserved owners. Per-connection retained activation traffic is
bounded by message-count and encoded-byte quotas; overflow follows the same
initial required/optional or post-startup connection-isolation policy.

Initial collision handling is independent of Ready order:
required/required conflicts fail startup; required/optional keeps the required
instance and disables the optional one; optional/optional disables every
claimant. A conflict with a harness-internal owner fails required startup or
disables an optional claimant. After the startup barrier, respawns and runtime
registrations are newcomers and cannot evict an incumbent. Changing an assigned
prefix requires restarting that extension instance.

One narrow bootstrap exception exists: an `ExtensionDataRequest` received before
that peer's `Ready` is handled immediately because an initial Configure handler
may need extension-owned storage before it can accept configuration. After that
peer sends `Ready`, the same RPC is operational traffic and remains globally
ordered behind any still-open activation barrier.

An extension that cannot parse or apply its `Configure.config` reports the
failure with `HarnessInputMessage::ConfigError`. The harness converts every
extension `ConfigError` into a mandatory `harness.notice`; it must not drop,
downgrade, or restrict the diagnostic to startup delivery. As specified by
[SPEC-tau-harness-event-processing](SPEC-tau-harness-event-processing.md), the
notice is replayable so both initial and late UI subscribers see configuration
failures, including failures reported before a terminal UI subscribes.

## Optional extension startup

Extension command resolution validates the command slot before flattening the
wrapper `prefix`, command, and `suffix` into process argv. A nonempty explicit
command is preserved. For a user-added entry with omitted `command` and a
nonempty `suffix`, the resolver uses the current Tau executable as the command
so renamed bundled-component instances can piggyback on Tau. Explicit
`command: []`, omitted command with an empty suffix, and prefix-only entries are
invalid; a wrapper prefix cannot become the executable. Built-in command
defaults and nonempty explicit custom commands retain their existing behavior.

Extension startup availability is controlled by resolved `ExtensionConfig.require`
and its validated per-instance `startup_timeout_seconds` (one through 3,600).
Each supervised child receives its own deadline from successful spawn until its
first `Ready`. Every externally managed or queued entry without such a record
receives the general deadline from one startup-wait instant, rather than a fresh
window after each event. Ordinary entries default to two seconds and built-ins
may select a longer documented default. A required entry that reaches its own
deadline fails startup closed. An optional entry that reaches its own deadline
is disabled with the existing mandatory replayable notice, while other entries
retain their independent deadlines.
Required extensions preserve startup-fatal behavior for harness-owned init
failures such as missing commands, missing required declared secrets, spawn
failure, and pre-Ready timeout. Other pre-Ready disconnect handling follows the
existing compatibility behavior unless the disconnect is already provider/socket
fatal. Optional extensions (`require: false`) are skipped or disabled for
startup/config/secret/pre-Ready failures, but the failure must still be emitted as
a mandatory replayable `harness.notice` so initial and late UI subscribers see why
the extension is absent. This policy is limited to startup/init availability; do
not treat it as a sandbox. Required pre-Ready protocol violations remain
startup-fatal; optional pre-Ready violations disable that peer. A malformed frame
is reported distinctly from clean EOF: required initial peers fail startup,
optional initial peers are disabled, and an already-live extension is isolated to
that connection and follows normal disconnect/respawn behavior rather than
terminating the harness. Diagnostics must
not leak secrets; mandatory/critical notices remain replayable and protected from
interceptor rewrite/drop, and extension-authored notices cannot spoof them.

Process spawn failures identify the configured extension instance and the
resolved executable from its `command` field using bounded, escaped values. An
explicitly configured cwd is included because either it or the executable may
have caused the operating-system spawn failure; an inherited cwd is not
invented. The underlying operating-system error remains in the error source
chain. Spawn diagnostics never retain or render command arguments, full
extension configuration, environment values, or resolved secret values.

## Extension availability startup data flow

`tau-config` owns strict parsing of the supported names-only
`TAU_ENABLE_EXTENSIONS` input without logging its raw value. The outer CLI parses
and validates it early for fresh-harness commands, preserving argv order for
subsequent CLI operations.
Normal launches pass only ordered CLI operations through the private,
unstable `TAU_EXTENSION_CLI_OVERRIDES` child transport; the daemon command
clears inherited transport when there are no operations. The spawned harness
decodes malformed private transport as a fatal startup error, fail-closed and
without logging raw values. Direct in-process `component harness`
dispatch passes the same typed operations explicitly and does not consult the
private transport for them. Harness settings own the canonical final resolver:
config, public environment named enables, then ordered CLI overrides.

Deterministic embedded and daemon acceptance may explicitly bypass all ambient
startup environment and CLI compatibility transports and require an exact
resolved extension-name allowlist before spawn. Both hermetic and normal startup
retain their single accepted effective settings snapshot for runtime baseline
lookups; neither mode rereads settings or performs live reload. Normal interactive
and default daemon startup use the ordered pipeline above.

## Environment enablement boundary

`TAU_ENABLE_EXTENSIONS` is trusted startup configuration: enabling an extension
may execute its configured program and expose configured local or external
boundaries. It accepts extension names only, not arguments, configuration, or
shell syntax. Do not place credentials in it (environment values may be visible
through process/service inspection); use Tau's secret mechanisms and run only
extensions you trust.
Every supervised extension crosses the fail-closed launch isolation boundary in
[SPEC-extension-secret-storage](../../../specs/SPEC-extension-secret-storage.md).
The harness installs user and mount namespaces outside the complete configured
argv, masks secrets before selecting cwd, and denies Secret authority to
test-only in-process connections.

## Ambient runtime indicators

Configured Tool/Core extensions may publish a complete bounded set of ambient
runtime indicators for each live agent. The harness aggregates current
configured-source contributions by union. It clears a source contribution when
that connection disconnects, clears an agent's contributions when the agent
unloads, and clears all contributions on session rollover; these transient
declarations never enter replay.
