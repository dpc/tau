# SPEC-tau-harness-event-processing: Event Processing

Changes to event persistence, sequencing authority, replay, or logging behavior
are subject to
[DECISION-persistence-and-extension-interface-change-approval](../../../specs/DECISION-persistence-and-extension-interface-change-approval.md).

## Event sequencing, interception, and persistence

All ordinary event publication should flow through the central publish path:
`enqueue_publish` runs interceptors in priority order, `commit_event` stamps a
single runtime sequence/timestamp, writes debug/event-log records, persists
eligible semantic facts, and broadcasts delivery frames. Direct calls to
`commit_event` are reserved for code that has already resolved interception.

Interceptors are local privileged extensions. They can inspect, modify, or drop
most matching events before commit. The harness protects selected facts as
must-pass and immutable because live state, durable resume state, and transcript
routing must agree. Fully immutable facts include session lifecycle facts,
session membership facts, `agent.started`, harness-owned agent message
projections, terminal tool completion facts (`tool.result`, `tool.error`,
`provider.tool_result`, `provider.tool_error`, `tool.cancelled`,
`tool.background_result`, and `tool.background_error`), and selected response
closure facts such as `provider.response_finished`. Prompt text facts are
must-pass, but only their routing keys are generally immutable: interceptors
may rewrite text on the sanctioned prompt-text events without changing agent
id, message class, or originator. A submitted or steered prompt tagged
`internal_kind=context_size_alert` additionally protects its tag and text so
durable history retains the exact configured alert. Mandatory `harness.notice`
diagnostics (critical notices
and `always_show` warnings such as extension config errors) are replayable,
published with a call-site `must_pass` override, and protected from interceptor
rewrite/drop.

`tool.request` and `tool.started` are eligible session-scoped execution restore
facts. Non-transient facts are persisted in each session's
`restore-events.cbor` stream (or the
equivalent in-memory stream for ephemeral sessions), replayed only to peers that
request matching `historical_selectors`, and deliberately kept out of agent
transcript logs. Live tool execution remains driven only by non-replay
`tool.started` deliveries.
For peer-authored requests, generic Emit preserves the supplied `transient`
value. Only non-transient requests enter the restore stream, where their source
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
commands, validated per-agent metadata set/unset facts, and a narrow fallback
allowlist. UI command handlers keep their existing keep-going result at the
dispatch-helper boundary; the outer client-message layer remains responsible for
connection lifetime. Metadata writes are validated and enqueued through the normal
publish path; fallback publication is limited to explicitly allowed UI/live
events and extension-owned custom events, using the explicit transient override
or the event default. Tool lifecycle/terminal facts and harness-owned lifecycle,
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
protocol-I/O stats are exposed only through the `ui.debug_event_stats_request`
control path, answered with a directed non-persisted notice, and must not add
subscriptions or synthetic replay events.

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
transient value. Full terminal alternatives and the intentionally non-transactional
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
restore facts only in same-daemon memory, while durable restore logs fail closed
on corrupt or semantically invalid existing records instead of being extended.

## Interceptor confidentiality

Interceptors are privileged local extensions. They can see, modify, or drop most
events they subscribe to before those events commit. Must-pass and immutable
checks protect selected harness-owned facts from integrity loss, but they are not
confidentiality boundaries: do not expose sensitive event streams to interceptors
you do not trust.

## Navigation-mode writes

The event loop consumes `ui.set_agent_navigation_mode` only from UI intake and
serializes accepted absolute writes, so the last accepted write wins. Extensions
cannot mutate this state. Harness-authored `agent.stats_updated` snapshots are
must-pass and immutable because they carry the complete shared classification.

## Directed agent-roster reads

`get_current_session` is accepted from an attached local socket/control
connection and returns the event loop's in-memory current session id directly to
that requester. Socket connections currently retain their accepted UI metadata;
the `Hello.client_kind` claim does not authorize this same-UID control RPC.
Runtime files locate the socket but do not supply the returned lifecycle fact.
See
[DECISION-current-session-control-rpc](../../../specs/DECISION-current-session-control-rpc.md).

`get_session_agent_list` uses the same harness-assigned local UI/control
connection metadata as `get_current_session`; `Hello.client_kind` does not
authorize it. Its result is correlated and sent only to the requester. It
bypasses event publication, interception, persistence,
subscriptions, and replay. The serialized event-loop position supplies one
coherent membership/runtime/navigation cut, although the result may become stale
immediately after delivery.
