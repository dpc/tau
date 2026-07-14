# SPEC-tau-harness-extension-lifecycle: Extension Lifecycle

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
identity.

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

Extensions that need to turn external input or internal wakeups into an agent
prompt use `extension.prompt_submit_request`. The harness accepts this request
only on the extension path, validates the target loaded agent, and then submits
a user-style or hidden internal prompt through the same machinery as UI prompt
intake. Internal extension prompts do not update user-interaction metadata, but
still wake queued agents. The durable transcript fact remains the harness-owned
`agent.prompt_submitted`; extensions may not forge prompt or message transcript
facts directly.

Cross-harness agent messages use the dedicated `ExternalAgentMessage` protocol
RPC, not `Emit`. The sender-side built-in `message` tool parses
bare `&<session-id>`, explicit `&<session-id>/@<agent-id>`, and legacy exact
`<session-id>/<agent-id>` addresses, treats the current session as local, mints a
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
configured entrypoint endpoint; exact capabilities preserve known-address
delivery to non-entrypoint agents. Authenticated peer bodies remain escaped
agent content in a typed peer-message prompt envelope, never harness
instructions.
Runtime-dir discovery verifies matching candidates by connecting to their
sockets. A failed probe is not enough to unlink discovery files while the
metadata pid is still live; dead-pid entries are eligible for cleanup on
platforms where Tau has a safe pid-liveness backend, so a transient probe
failure does not permanently hide a running daemon.

## Extension configuration errors

After `Hello`, the harness sends `Configure` before accepting declarations and
`Ready`. Its optional tool prefix is retained with the configured instance
across respawn. Registrations from a prefixed instance must place internal
names, visible aliases, and groups inside the assigned exact component envelope.
Final internal-name ownership is unique across live connections; prompt
snapshots separately reject simultaneously visible alias collisions. These
rules implement
[DESIGN-extension-tool-prefixes](../../../specs/DESIGN-extension-tool-prefixes.md).
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
Harness-internal tool handlers are installed before this preflight, so their names
participate as reserved owners. Per-connection retained activation traffic is
bounded by message-count and encoded-byte quotas; overflow follows the same
initial required/optional or post-startup connection-isolation policy.

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

Extension startup availability is controlled by resolved `ExtensionConfig.require`.
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

## Extension availability startup data flow

`tau-config` owns strict parsing of the supported names-only
`TAU_ENABLE_EXTENSIONS` input. The outer CLI parses and validates it early for
fresh-harness commands, preserving argv order for subsequent CLI operations.
Normal launches pass only ordered CLI operations through the private,
unstable `TAU_EXTENSION_CLI_OVERRIDES` child transport; the daemon command
clears inherited transport when there are no operations. The spawned harness
decodes that transport fail-closed. Direct in-process `component harness`
dispatch passes the same typed operations explicitly and does not consult the
private transport for them. Harness settings own the canonical final resolver:
config, public environment named enables, then ordered CLI overrides.

## Environment enablement boundary

`TAU_ENABLE_EXTENSIONS` is trusted startup configuration: enabling an extension
may execute its configured program and expose configured local or external
boundaries. It accepts extension names only, not arguments, configuration, or
shell syntax. Do not place credentials in it (environment values may be visible
through process/service inspection); use Tau's secret mechanisms and run only
extensions you trust.
