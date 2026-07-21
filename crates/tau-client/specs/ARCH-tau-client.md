# ARCH-tau-client: tau-client architecture

Architectural or externally meaningful functional changes to this shared
harness-extension interface require the separately reviewed, human-confirmed
decision mandated by
[DECISION-persistence-and-extension-interface-change-approval](../../../specs/DECISION-persistence-and-extension-interface-change-approval.md).

`tau-client` is the shared runtime for Tau extension protocol peers. It sits
above `tau-proto`, which owns the wire messages. First-party extensions now use
`tau-client` directly; the former compatibility startup helper crate was removed
after the migration completed without a protocol break.

Configure supplies the stable extension instance name used by instance-scoped
state such as per-agent workdir metadata. The client runtime exposes committed
metadata events and context publication to the extension; it does not choose a
shared filesystem namespace or seed workdir state across instances. See
[SPEC-per-agent-extension-workdirs](../../../specs/SPEC-per-agent-extension-workdirs.md).

The runner writes `Hello`, requires the initial harness `Configure`, installs an
immutable logical-to-wire tool-name scope, constructs state, and dispatches
initial Configure handlers exactly once. It then writes static declarations,
accepted Configure-derived declarations, and `Ready`, in that order.
`startup_event` preserves durable `Emit` metadata, while
`startup_transient_event` sets `Emit.transient` for runtime-only declarations;
both retain their builder order before `Ready`.
Tool helpers always publish transient `tool.registration_declared` /
`tool.unregistration_declared` inputs rather than harness-owned canonical state;
the complete downstream contract is
[SPEC-tool-declarations-and-canonical-state](../../../specs/SPEC-tool-declarations-and-canonical-state.md).
First-party Tool/Core producers submit transient `tool.progress_reported`
observations rather than harness-owned `tool.progress` facts. Submission only
acknowledges local writer admission/flush; interception and downstream
routed-call validation determine whether the harness later publishes canonical
progress. See
[SPEC-tool-progress-reports-and-canonical-facts](../../../specs/SPEC-tool-progress-reports-and-canonical-facts.md).
Terminal Tool/Core producers use the typed result, error, and cancellation
report helpers. `ToolTerminalOutcome` centralizes conversion from pure
canonical-shaped dispatch outcomes to `tool.*_reported` wire events so
production handles and test-channel adapters cannot drift. Contextual tool
helpers bind the routed call id, final wire name, and originator where present;
raw `ClientHandle` helpers deliberately perform no correlation or name scoping.
All report helpers set explicit transient metadata. Generic `emit` remains
wire-mechanical and does not rewrite canonical names. Local flush or queue
admission does not acknowledge report commit, routed-call validation, or
canonical completion. See
[SPEC-terminal-tool-reports-and-canonical-outcomes](../../../specs/SPEC-terminal-tool-reports-and-canonical-outcomes.md).
Configure-derived declarations are buffered while handlers run so they override
same-name static defaults without becoming visible before configuration is
accepted. Rejection emits `ConfigError`, discards buffered declarations, and
withholds `Ready`; any `ConfigError` emitted after `Hello` and before `Ready`,
including from a state factory, rejects the same startup transaction. Ordinary
manual startup returns that rejection as an error rather than returning a
runtime that never became ready. Later configuration may change extension-owned
settings but not the tool prefix. This implements
[DECISION-extension-tool-prefixes](../../../specs/DECISION-extension-tool-prefixes.md).

The manual-loop runtime uses the same startup writer and dispatch machinery, but
hands receive-loop ownership to the extension. It starts a reader thread, exposes
`recv`/`recv_timeout` for blocking or timer-driven receive loops, exposes
`try_recv`/`wait_for_wake` plus `ManualRuntimeWaker` for reactive loops that
combine harness input with extension-owned side channels, and exposes
`dispatch_one` for decoded messages. Wake notifications are coalesced and
payload-free: the reader thread wakes after queueing harness input/EOF/errors,
and extension workers must enqueue their own side-channel work before calling a
cloned waker. Reactive callers are responsible for draining all ready sources
fairly after each wake, because one wake may represent multiple ready items.
Extension state and handlers stay on the caller thread; only the protocol reader
and writer need to move to background threads.

The builder preserves the first-seen order of startup subscription selectors
while coalescing exact structural duplicates. It does not collapse logical
overlaps such as `Prefix("tool.")` plus an exact `tool.started` selector.

Outbound frames go through a writer thread owned by `TauExtensionRunner`.
`ClientHandle` is cloneable, serializes writes through that thread, and normally
waits for each frame to be encoded and flushed before returning. During initial
Configure dispatch, capability declarations return after entering the startup
buffer; the runner later reports their encode/flush failure while draining that
buffer before `Ready`. A closed or panicked
writer is reported as `ClientError`. Detached enqueue helpers are also available
for background workers that must not block the protocol reader on output
backpressure; after admission checks, those helpers report queue-closed failures
to the caller and let the writer thread own any later encode/flush error. `Ready`
is runner-owned, linearized with pre-Ready `ConfigError`, and rejected by raw
synchronous or detached handle APIs rather than being admitted as ordinary
output.

`ProtocolIoMeter` is a protocol-mechanical frame counter shared by UI and
extension transports. It groups already-decoded/encoded frames by delivered or
emitted event name, or by `message.*` for non-event protocol frames, and records
encoded byte counts. The default meter used by extension transports remains
cumulative-only; the interactive UI explicitly opts into detailed diagnostics.
Those diagnostics classify two orthogonal axes: the initial connection remains in
`cold-attach` through the non-replay `session.replay_complete` boundary, while
each delivery independently retains its replay/non-replay marker. Later
per-agent replay therefore appears as `steady.replay`, not cold attach. For
selected high-volume events the diagnostics retain only byte-size
distributions and equality classifications: tool-result encoded frame, full bare
event, raw result, display, provider components, and missing-display occurrences;
final-response authoritative semantic output, provider replay sidecars, and a
metadata-only event projection; exact agent-stats duplicates within the observed
loaded epoch; and
exact/sequence-only/substantive quota changes. Agent comparison state resets on
load, unload, and session start; quota comparison state resets on session start.
Equality caches are also separated by attach and replay dimensions, so a replay
snapshot is not counted as a duplicate predecessor for the first live snapshot.
These diagnostics do not log payload content, suppress events, or change event
semantics. Component encodings are independent CBOR size measurements and are not
expected to add up to the enclosing frame.

The meter does not subscribe to events, affect replay/live filtering, own
lifecycle policy, or call back into the harness. The harness may depend on this
protocol-I/O utility surface without depending on tau-client runner or extension
lifecycle abstractions. Per-direction keys are capped and overflow into an
`other` bucket so extension-owned custom event names cannot grow the harness's
debug accounting state without bound. Detailed measurement keys are a fixed set
derived from built-in event types, and percentile histograms use fixed
logarithmic buckets rather than retaining individual samples.

Extensions that intentionally leave background workers running after disconnect
can opt into a detached-writer run mode. That mode preserves startup and handler
error reporting but does not join the writer at shutdown, so harness
`Disconnect` latency does not depend on queued background output.

Extensions whose background workers need a persistent outbound handle can use
the detached-writer state-factory entry point. The factory runs after scope
installation and before Configure dispatch; synchronous public output remains
startup-gated and detached output is held until `Ready`.

Manual-loop extensions use the same startup staging. Their state factory runs
before Configure dispatch, with the same output gates, and timer branches can
use `ManualExtensionRuntime`'s separate `handle()` method.

Config-gated extensions that cannot know their startup declarations until after
an initial `Configure` can use the deferred manual-startup entry point. That
mode writes and flushes only `Hello`, starts the reader/writer threads, and
returns before any `Subscribe`, `Intercept`, startup `Emit`, or `Ready` frames.
The caller then receives configuration, sends explicit dynamic startup frames
through `ManualExtensionRuntime` helpers (including `startup_local_tool` for
scope-aware logical registrations), and completes startup exactly once with
`startup_ready`. Static builder declarations are rejected in this mode so a
config-gated extension cannot accidentally leak pre-configuration subscriptions,
intercepts, tool registrations, action schemas, or ready text. After `Ready`,
the runtime has the same blocking receive, reactive wake, dispatch, finish, and
detached-finish contracts as other manual-loop users.

Event handlers are either typed payload handlers or raw delivery handlers. Typed
handlers cover the built-in `EventPayload` variants, including common runtime
events needed by extensions that fold session, agent metadata, cancellation, and
side-agent result state; raw handlers are available for unsupported first-party
or custom extension events. `on`/`on_raw` and `on_live`/`on_raw_live` populate
`live_selectors` and skip replay-marked deliveries. `on_restore` and
`on_raw_restore` populate `historical_selectors` and run only for replay-marked
deliveries, which include both durable restore facts and catch-up snapshots such
as `session.agent_loaded` or `harness.session_dir`. Tool and action helpers are
live-only side-effect handlers.

Raw handlers normally add their selector to the startup subscription set. For
deliveries that the harness routes through another protocol contract, such as
provider-kind prompt deliveries, routed raw handlers reuse the same dispatch and
replay filtering without adding a startup subscription. This keeps provider
direct-routing support from broadening replay or broadcast event access.

Configuration handlers deserialize the CBOR configuration into the requested
type. Decode failures and handler application errors emit `ConfigError` frames
and the runner continues processing later messages. Handlers can register a
configuration-error hook for fail-closed cleanup; that hook runs before
`ConfigError` is emitted for both typed decode failures and application
failures.

Extensions whose lifecycle policy must run before typed decoding can register a
raw configuration handler. Raw handlers receive the original `Configure` message
and can parse it explicitly after checking runtime state; returned errors still
emit one `ConfigError` and do not stop the message loop.

Action helpers mirror the startup/dispatch split used by tools. `publish_actions`
emits an `action.schema_published` startup event before `Ready`, and action
handlers subscribe to `action.invoke` while dispatching only live deliveries whose
action id matches the declaration. Extension/instance-level action routing
remains a harness responsibility because configured instance names can differ from
protocol `Hello` names; tau-client does not broaden subscriptions or process
replay-marked action invocations.

Context-provider helpers are startup publication helpers only. They emit the
existing `extension.context_provider_register`,
`extension.session_context_provider_register`, and
`extension.prompt_fragment_publish` DTOs before `Ready` without owning the
runtime lifecycle: extensions still subscribe to session/agent events, fold any
state they need, publish context values, and choose when to emit
`extension.context_ready` or `extension.session_context_ready`. `ClientHandle`
provides small readiness emit helpers for those two DTOs, but readiness policy
and correlation remain extension-owned.
The session-provider registration helper and session-readiness helper emit transient
wire publications. Session skill and AGENTS.md producers must do the same; the harness
commits them before downstream effects under
[SPEC-session-discovery-declarations-and-readiness](../../../specs/SPEC-session-discovery-declarations-and-readiness.md).
The per-agent registration and readiness helpers also emit transient wire
publications. Per-agent value producers use transient publication; the harness
commits all three events before projection or wait release under
[SPEC-per-agent-context-declarations-and-readiness](../../../specs/SPEC-per-agent-context-declarations-and-readiness.md).
Prompt-fragment startup events are transient declarations: the harness commits
them through ordinary interception before activating their prompt projection.
See
[SPEC-prompt-fragment-declarations-and-projection](../../../specs/SPEC-prompt-fragment-declarations-and-projection.md).

Manual-loop extensions that need harness-owned extension-data storage can use the
extension-data RPC helper. It generates a request id, sends the existing
`ExtensionDataRequest` frame, waits for the matching `ExtensionDataResult`, and
buffers unrelated harness frames back into the manual runtime so later
`recv`/`dispatch_one` calls still see them in order. The helper does not add
storage policy, path validation, or background demux ownership to tau-client; the
harness still owns storage boundaries and the extension still owns how storage
errors map to feature behavior.

Manual-loop receive results distinguish timeout, clean input EOF, and protocol
`Disconnect`. Non-blocking `try_recv` has separate message, input-closed, and
empty states because timeouts are only possible for `recv_timeout`. Clean input
EOF allows the caller to keep running local timers or side-channel completions
and emit post-EOF output before graceful writer shutdown. `finish()` always shuts
down and joins the writer; it joins the reader only after EOF or another already
finished reader state, because arbitrary blocking `Read` implementations cannot
be cancelled portably. If a caller stops before EOF, the blocked reader is
detached and exits after its next read completes. Protocol `Disconnect` is a
dispatch outcome; callers that may have blocked or long-lived background output,
or a still-open input stream during disconnect, can choose detached finish, which
returns state without shutting down or joining protocol threads.

Intercept handlers always produce exactly one `InterceptReply` for each request.
If the handler fails, the runner sends a pass-through reply first, then returns
the handler error so the extension run stops without leaving the harness waiting.

## Testing strategy

Tests in `src/tests.rs` are protocol contract tests for the reusable runtime.
They should cover startup frame ordering, subscription selector semantics,
writer-thread behavior, configuration errors, replay/live dispatch boundaries,
raw event dispatch, tool/action matching, intercept reply guarantees, context
provider startup publication, disconnect behavior, manual-loop
receive/dispatch/shutdown contracts, deferred manual startup, extension-data RPC
demux, builder validation, empty subscriptions, and plugin composition. Add
focused coverage when changing lifecycle, writer shutdown, replay filtering,
configuration, intercept, action dispatch, manual receive loops, extension-data
request correlation, or startup declaration behavior so migrated extensions do
not need to rediscover runtime regressions independently. Focused
`protocol_io/tests.rs` and `protocol_io/diagnostics/tests.rs` coverage owns
cumulative-counter compatibility, cold-attach boundaries, replay/non-replay
classification, selected payload attribution, bounded histograms, and
equality-cache reset boundaries.
