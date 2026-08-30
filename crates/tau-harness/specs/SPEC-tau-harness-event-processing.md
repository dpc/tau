# SPEC-tau-harness-event-processing: Event Processing

Durable semantic facts cross persistence admission before EventLog, debug JSONL,
subscriptions, reactions, and continuation mutation. `Full`, unavailable, stale,
or staging failures leave those consumers unchanged. Worker I/O follows
acceptance; retryable failures retain the FIFO head, and rejected
`ShellCommandFinished` facts receive no substitute canonical broadcast.

## Record justification

Publication spans harness intake, admission and activation staging, interceptor
selection, debug logging, semantic stores, broadcast, replay, and downstream
domain consumers. No one owning module can state the complete commit and
persistence contract coherently.

Changes to event persistence, sequencing authority, replay, or logging behavior
are subject to
[GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

## Event sequencing, interception, and persistence

[SPEC-peer-event-publication](../../../specs/SPEC-peer-event-publication.md)
governs publication metadata; event-family exclusions and persistence exceptions
remain independent.

All ordinary event publication should flow through the central publish path:
`enqueue_publish` runs interceptors in priority order, `commit_event` stamps a
single runtime sequence/timestamp, queues debug records, writes event-log
records, persists
eligible semantic facts, and broadcasts delivery frames. Direct calls to
`commit_event` are reserved for code that has already resolved interception.
Extension prompt-fragment projection runs only after this ordinary commit.

Live output uses one process-local logical stream governed by
[GATE-runtime-live-event-log-cursors](../../../specs/GATE-runtime-live-event-log-cursors.md).
Routing freezes eligible connection generations when it admits each frame.
Each writer follows the stream from its own runtime cursor and advances only
after successful encoding, write, flush, and downlink metering; retirement
releases that generation's remaining obligations. The stream prunes only
through the lowest active cursor. Replay establishes a live-tail barrier before
catch-up output and releases the follower after the replay-complete boundary.
Runtime positions neither enter semantic journals nor appear on the wire.

`tau_harness::commit_timing` traces each accepted non-message commit's exact
monotonic microsecond total plus debug-log, semantic-persistence, bus-enqueue,
post-commit, and residual/unattributed phases. It carries only event name,
terminal result class, and durations. Cycles over 500 milliseconds emit the
same fields at warning level. This operational tracing never enters
`events.jsonl`, changes commit ordering, or affects persistence/publication.

`tau_harness::prompt_acceptance` additionally follows one authenticated,
immediately dispatched Human UI prompt through interception and publication
admission. It owns exactly one content-free, process-local terminal observation;
queued, replayed, internal, and non-UI work does not create that owner. Its
terminal does not alter protocol, persistence, replay, watcher behavior,
extension interfaces, or publication authority. See the [operator-facing fixed
schema and phase meanings](../../../crates/tau-skills/self-knowledge/tau-self-knowledge-debugging.md#interactive-prompt-latency-traces).

Eligible debug JSONL observations serialize one complete line and attempt
immediate admission to the process-wide bounded writer queue. File locking,
opening, EOF lookup, append, flush, and rollback run only on the detached writer
thread and never gate semantic persistence, publication, lifecycle, or process
exit. Queue contention or capacity exhaustion drops only that diagnostic line.

When lifecycle teardown destructively cancels an intercepted publication, the
harness temporarily skips that interceptor registration until it consumes the
single outstanding stale reply. This preserves the extension connection and
registration while preventing an uncorrelated old reply from applying to a
later publication. Replacement registration is accepted while suspended, no
timeout applies, exactly one reply is consumed without action, and disconnect
clears the suspension; normal interception resumes after reply consumption or a
new connection.
Deferred publications removed by teardown run the same
reservation, ACK, and ephemeral-marker cleanup as an interceptor Drop.
Rollover advances the admission generation before quiescing publication.
Configured-extension frames retain their original admission session/generation
through pre-Ready and global activation staging. Documented session-bound raw
observation families still commit and broadcast across rollover: tool requests,
start-agent requests, internal-prompt requests, and every per-agent-context and
session-discovery declaration/readiness event. One common post-commit boundary
rejects their stale-generation semantics, including tool routing, prompt
submission, canonical message projection, metadata mutation, and
context/discovery projection, while releasing any activation reservation.
Tool declarations, prompt-fragment declarations, provider-model declarations,
and provider-quota reports are process-global exceptions: rollover retains them
and their semantic consumer runs only when the captured connection and extension
instance still exactly match the live generation. The family-specific linked
specifications are the source of truth for whether a committed peer event is
session-bound observation or process-global current state.
Declarations are excluded from semantic persistence and replay for either
caller-supplied `persist` value; see
[SPEC-prompt-fragment-declarations-and-projection](../../../specs/SPEC-prompt-fragment-declarations-and-projection.md).
Session registration, complete discovery snapshots, canonical projection, and
readiness release likewise run only after ordinary commit; their raw events are
excluded from semantic persistence for either caller `persist` value. See
[SPEC-session-discovery-declarations-and-readiness](../../../specs/SPEC-session-discovery-declarations-and-readiness.md).
Per-agent context registration, correlated discovery/value projection, and
readiness release also run only after ordinary commit. The raw events are excluded from semantic
persistence for either caller `persist` value. See
[SPEC-per-agent-context-declarations-and-readiness](../../../specs/SPEC-per-agent-context-declarations-and-readiness.md).
Internal-prompt requests likewise cross ordinary commit before target
validation and prompt submission. Raw requests remain outside semantic history
for either caller `persist` value. See
[SPEC-internal-prompt-submit-requests](../../../specs/SPEC-internal-prompt-submit-requests.md).
Start-agent requests also commit before validation, duplicate rebinding,
acceptance/result routing, and child creation. Raw requests remain outside
semantic history for either caller `persist` value. See
[SPEC-start-agent-requests](../../../specs/SPEC-start-agent-requests.md).
Terminal-output events likewise use ordinary interception and commit before a
subscribed UI acts. They remain outside semantic history for either caller
`persist` value, and terminal consumers reject replay delivery. See
[SPEC-terminal-output-side-effect-events](../../../specs/SPEC-terminal-output-side-effect-events.md).
Custom extension-owned events also use ordinary interception and commit before
direct subscriber delivery. Opaque custom events remain outside semantic history
for either caller `persist` value. See
[SPEC-custom-extension-events](../../../specs/SPEC-custom-extension-events.md).
Attached-UI prompt-draft and focus observations likewise use ordinary
interception and commit before live subscriber reaction. Both remain outside
semantic history for either caller `persist` value. See
[SPEC-ui-prompt-draft-and-focus-events](../../../specs/SPEC-ui-prompt-draft-and-focus-events.md).
Tool/Core user-shell reports also cross ordinary interception and commit before
the downstream consumer revalidates captured generation/session and private
route identity. Reports remain outside semantic history; only the harness
publishes canonical progress/completion. See
[SPEC-shell-command-reports-and-canonical-facts](../../../specs/SPEC-shell-command-reports-and-canonical-facts.md).
Tool/Core ambient-indicator declarations likewise cross ordinary interception
and commit before current-generation and live-agent revalidation updates the
harness-owned transient per-source contribution. They never enter semantic
history or replay.
Dedicated configured-extension notice requests are handled inline and converted
to harness-authored `extension.notice` events. The request carries only message
and level; the harness caps critical to warning and fixes source, kind,
`purpose = diagnostic`, and live-only publication. The resulting event uses ordinary
interception, commit, and live broadcast but never semantic persistence or
replay. Debug JSONL and protocol metering retain the raw
`message.extension_notice_request` input separately from the later published
event. See
[SPEC-extension-notice-requests](../../../specs/SPEC-extension-notice-requests.md).

Interceptors are local privileged extensions. They can inspect, modify, or drop
most matching events before commit. The harness protects selected facts as
must-pass and immutable because live state, durable resume state, and transcript
routing must agree. Fully immutable facts include session lifecycle facts,
session membership facts, `agent.started`, harness-owned agent message
projections, terminal tool completion facts (`tool.result`,
`tool.result_display`, `tool.error`,
`provider.tool_result`, `provider.tool_error`, `tool.cancelled`,
`tool.background_result`, `tool.background_result_display`, and
`tool.background_error`), and selected response
closure facts such as `provider.response_finished`. Prompt text facts are
must-pass, but only their routing keys are generally immutable: interceptors
may rewrite text on the sanctioned prompt-text events without changing agent
id, message class, or originator. A submitted or steered prompt tagged
`internal_kind=context_size_alert` or `background_tool_completion` additionally
protects its tag and text so durable history retains the exact harness-authored
alert or lifecycle notice. Mandatory `harness.notice` alerts (and critical
notices regardless of purpose) are replayable,
published with a call-site `must_pass` override, and protected from interceptor
rewrite/drop.

A connected interceptor may intentionally leave an intercept request
unanswered and thereby stall that publication plus every globally serialized
publication behind it indefinitely. The harness deliberately applies no
timeout, admission budget, lag quarantine/disconnect, rejection, or
backpressure policy to `pending_intercept` or `deferred_publishes`. Deferred
publications retain their full events while stalled and can therefore consume
memory without bound. This is an accepted consequence of granting trusted
interceptors authority to stop publication pending an explicit decision.
Canonical `shell.command_finished` is likewise immutable and must-pass so UI
completion and optional post-commit transcript injection cannot diverge.
Canonical shell progress retains its harness-owned mapped command/target
identity while allowing the established chunk/stream interception changes.

Committed harness-owned agent-message projections are their owning
transcript's sole payload occurrence. Their post-commit reaction may install a
runtime-only sequence wake but never submits or steers a second payload prompt.
Sequence-aware fold placement, provider rendering, replay, and acknowledgement
are specified by
[SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md).

`tool.request` and `tool.started` are eligible session-scoped execution restore
facts. Publications with `persist=true` enter each session's
`restore-events.cbor` stream (or the
equivalent in-memory stream for ephemeral sessions), replayed only to peers that
request matching `historical_selectors`, and deliberately kept out of agent
transcript logs. Live tool execution remains driven only by non-replay
`tool.started` deliveries.
For peer-authored requests, generic Emit preserves the supplied `persist`
value. Only requests with `persist=true` enter the restore stream, where their source
is the stable configured publisher name rather than the run-local connection.
The live committed peer envelope alone invokes correlation and routing;
historical delivery never does. The complete peer request flow is
[SPEC-tool-requests-and-routing](../../../specs/SPEC-tool-requests-and-routing.md).

Catch-up snapshots reconstructed from current harness state (for example
`session.agent_loaded`, folded metadata, and `harness.session_dir`) are also
selected by `historical_selectors` and delivered with `EventDelivery.replay =
true`. Only `agent.replay_complete` and `session.replay_complete` boundaries
remain non-replay during catch-up; live delivery is buffered until the session
boundary has been sent.

## Client event boundary

UI clients are local UI/control peers, not providers. Client `emit` intake must
preserve provider ownership by routing provider-category events through the
extension/provider event path, where provider-source and prompt-owner validation
still apply. Non-provider client events are partitioned into harness-owned UI
commands, per-agent metadata set/unset requests, and a narrow fallback allowlist.
UI command handlers keep their existing keep-going result at the dispatch-helper
boundary; the outer client-message layer remains responsible for connection
lifetime. Metadata requests use exact attached-socket-UI authority, commit before
validation, and produce separate harness-authored canonical facts; fallback
publication is limited to explicitly allowed UI/live events. Prompt-draft, focus,
and extension-owned custom events use exact attached-UI authority rather
than fallback and preserve the explicit persistence override or event default.
Metadata request replacements preserve correlation identity where applicable,
then commit and validate downstream.
Tool lifecycle/terminal facts and harness-owned lifecycle,
membership, transcript, and status facts must not be accepted through client
fallback.

Configured Tool/Core peers publish terminal outcomes only as transient
`tool.result_reported`, `tool.error_reported`, or
`tool.cancelled_reported` observations. They commit through ordinary
interception before the downstream consumer validates the captured exact route
and live configured generation. Canonical terminal/provider/background
projections use the harness source and retain the immutable must-pass policy
described above. Reports and raw renderer result/error projections do not enter
semantic history; provider and cancellation/background facts retain their
existing persistence and replay behavior. See
[SPEC-terminal-tool-reports-and-canonical-outcomes](../../../specs/SPEC-terminal-tool-reports-and-canonical-outcomes.md).

UI debug/status commands that inspect local transport counters are direct live
responses to the requesting UI, not ordinary publish/replay traffic. Extension
protocol-I/O stats are exposed only through the flat
`ui_debug_event_stats_request` input message from an attached socket UI, answered
with a directed non-persisted notice, and must not add debug JSONL, subscriptions,
interception, or synthetic replay events. Authority uses the exact
`is_attached_socket_ui` classification: the harness assigned UI kind, socket
origin, and no dedicated external-message-peer role. Every unauthorized
non-extension client receives exactly one requester-directed, content-free
`ui.command_error`; configured extensions are silently denied without a response,
warning, or disconnection.

UI detach is a payload-free `ui_detach_request` input message from an attached
socket UI. During startup it records that the initial UI requested detach;
during the runtime serve loop it clears `exit_on_disconnect`. The request does
not enter publication, interception, subscriptions, semantic persistence, or
replay. It remains visible as a point-to-point input frame in local debug JSONL
and is metered as `message.ui_detach_request`.

Detach authority uses the same exact `is_attached_socket_ui` classification as
UI diagnostics. Other client origins are silently denied without changing
connection-control state or publishing a diagnostic. Configured extensions are
also silently denied after normal phase validation and metering but before
activation staging, so repeated requests cannot consume activation quota,
disconnect the extension, or fail required startup.

UI shutdown is a payload-free `ui_shutdown_request` input message. During
startup it records a pending shutdown; during the runtime serve loop it requests
unconditional harness shutdown. The loop then follows the same canonical shutdown path as a
termination signal: it retires admission, publishes the harness-authored
`session.shutdown` fact, disconnects all attached UIs and extensions, settles
semantic persistence, and cleans owned runtime discovery artifacts. The request
does not alter `exit_on_disconnect` and does not enter publication,
interception, subscriptions, semantic persistence, or replay. It remains
visible as a point-to-point input frame in local debug JSONL and is metered as
`message.ui_shutdown_request`.

After the terminal commits, all attached UI writers receive one concurrent
100-millisecond best-effort delivery grace. Tau then cancels their socket I/O
and continues cleanup, so a paused or non-reading UI cannot block shutdown.
Generic initial-UI transports have no cancellation primitive, but their close
workers still stop waiting at the same deadline.

Shutdown authority uses the exact `is_attached_socket_ui` classification.
Other client origins and configured extensions are silently denied under the
same phase-validation, metering, and activation-staging rules as detach.

UI tree inspection is a flat `ui_tree_request` input message from an attached
socket UI. The harness preserves the existing session/agent validation, prompt
anchor ordering, selected-head markers, and diagnostic text. It presents prompt
previews under the exact terminal-inert encoding contract in
[SPEC-tau-harness-session-state](SPEC-tau-harness-session-state.md), then
returns exactly one requester-directed multiline `harness.notice` with the
existing lines in their existing order. Neither the request nor result enters
publication, interception, subscriptions, semantic persistence, or replay. The
request remains visible in local debug JSONL and is metered as
`message.ui_tree_request`.

Tree authority uses the same exact `is_attached_socket_ui` classification.
Other client origins are silently denied so agent prompt previews cannot leak.
Configured extensions are silently denied after normal phase validation and
metering but before activation staging; illegal-phase requests retain normal
protocol-failure behavior.

## Harness-owned tool-call id scoping

The harness-owned `wait` and `cancel` tools treat explicit `tool_call_id`
arguments as scoped to the calling conversation. Exact `wait` requests must check
that the target call is owned by the waiting conversation before duplicate-wait,
queued-input preemption, or stored-result handling. `cancel` requests must check
that the target call is owned by the cancelling conversation before consulting
duplicate-cancel or completed-call state and before publishing
`tool.cancel_request`. Cross-owner probes use the same unknown-id behavior as
absent calls so tool-call existence, completion state, and already-cancelled
state do not leak across agents.

## Lifecycle events

Harness lifecycle events such as session start/shutdown and extension status are
normal events unless specifically marked must-pass/immutable. Session lifecycle
facts are protected because extensions and context providers use them to set up
or tear down per-session state. Extension lifecycle/status events are runtime
observability facts and may be intercepted like other non-protected events unless
call-site policy says otherwise.

## Provider response update routing

Configured Provider peers submit the five provider execution `_reported` events through
generic Emit. Reports commit before the harness validates the captured current
generation and prompt/retry correlation. Canonical provider facts and directed retry
outcomes use harness source; reports remain outside semantic history for either supplied
`persist` value. Full terminal alternatives and the intentionally non-transactional
report-to-canonical boundary are specified by
[SPEC-provider-execution-reports-and-canonical-facts](../../../specs/SPEC-provider-execution-reports-and-canonical-facts.md).

The harness treats canonical `provider.response_updated` as non-durable live
progress. After the corresponding report commits, it validates that the captured
Provider source owns the in-flight prompt, overwrites `agent_id` from harness
prompt ownership, enriches best-effort compaction metadata, and publishes the
canonical public update. Displayable deltas, status, compaction, and content-free
`response_stats` are all public provider-owned transient fields. Stats-only
reports are valid and must produce canonical updates so UIs can render response
liveness directly.

## Subscription and replay exposure

Event subscriptions are also a data-exposure and resource boundary. Peers should
subscribe to exact event names by default so new protocol events do not
silently expand live delivery, replay catch-up, high-volume traffic, or access to
sensitive/contentful payloads. Prefix/category subscriptions should be reserved
for intentionally generic observers that truly need the entire category; changes
to subscribers must consider replay behavior, payload size/frequency, and whether
the selected events carry prompt, tool, provider, or extension-provided content.
Historical selectors are a separate exposure decision from live selectors:
replayed `tool.request` and `tool.started` facts can include tool arguments and
must only be requested by restore code that needs them. Live execution handlers
must remain live-only and must not run from replayed delivery envelopes.
Historical catch-up also includes replay-marked current-state snapshots (for
example loaded-agent, metadata, and session-dir facts), with live events buffered
until the non-replay replay-complete boundary. Ephemeral session stores keep
restore facts only in same-daemon memory. A journal-backed writer recovers
restore facts under the session lock by truncating only an incomplete EOF frame;
a complete invalid frame fails closed unchanged before append. Read-only
historical replay remains strict.

## Interceptor confidentiality

Interceptors are privileged local extensions. They can see, modify, or drop most
events they subscribe to before those events commit. Must-pass and immutable
checks protect selected harness-owned facts from integrity loss, but they are not
confidentiality boundaries: do not expose sensitive event streams to interceptors
you do not trust.

## Navigation-mode writes

The event loop consumes `ui.set_agent_navigation_mode` only from UI intake and
serializes accepted absolute writes, so the last accepted write wins. It also
authenticates visible human `ui.prompt_submitted` intake and, after target/skill
validation and durable `agent.user_interaction_recorded` append, applies an
implicit absolute `active` write and enqueues fresh complete stats before queue
or dispatch. The implicit write emits no explicit-navigation result.
After durable creation and current-session membership setup, the internal
authenticated bare peer-entrypoint auto-start path likewise writes `active` and
publishes complete stats for only its newly created endpoint.

Rejected, internal, extension-originated, stale, unloaded, unavailable, or
terminating targets do not mutate navigation. Later queue dispatch, steering,
interception completion, and replay do not reapply the write. Extensions cannot
mutate this state. Harness-authored `agent.stats_updated` snapshots are must-pass
and immutable because they carry the complete shared classification.

Those transient snapshots also carry the loaded agent's runtime-only estimated
equivalent API cost. The harness prices each accepted provider usage record
incrementally with that record's provider-qualified serving model, conservatively
treats missing cached-token detail as uncached input, and saturates fixed-point
accumulation. Loading a fresh runtime agent starts at zero; durable replay does
not reconstruct or reprice historical usage.

## Directed agent-roster reads

`get_current_session` is accepted from an attached local socket/control
connection and returns the event loop's in-memory current session id plus its
immutable canonical startup project root directly to that requester. Socket
connections currently retain their accepted UI metadata; the `Hello.client_kind`
claim does not authorize this same-UID control RPC. Runtime files locate the
socket but supply neither returned field. The wire contract is specified by
[SPEC-tau-proto-session-events](../../tau-proto/specs/SPEC-tau-proto-session-events.md).

`get_session_agent_list` uses the same harness-assigned local UI/control
connection metadata as `get_current_session`; `Hello.client_kind` does not
authorize it. Its result is correlated and sent only to the requester. It
bypasses event publication, interception, persistence,
subscriptions, and replay. The serialized event-loop position supplies one
coherent membership/runtime/navigation cut, although the result may become stale
immediately after delivery.
