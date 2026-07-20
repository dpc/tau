# DECISION-generic-peer-event-emission: Generic peer event emission

Authority: confirmed, 2026-07-19, dpc

## Status

The peer protocol and harness have not yet completed this transition.
`HarnessInputMessage::Emit` currently enters peer-kind and event-specific intake
dispatchers that can validate, rewrite, suppress, route, or act on the nested
event before ordinary publication. The intended transition removes those
semantics from `Emit` intake and moves them to committed-event consumers or
explicit point-to-point protocol messages.

This decision applies to `Emit` from every peer kind, including extensions,
providers, UI clients, external peers, and harness-connected core components.

`HarnessInputMessage::Emit` is only the generic peer request to publish its
nested event. The harness authenticates the peer, applies generic admission and
resource policy, runs ordinary interception, commits the event with
harness-owned sequencing and source metadata, and broadcasts the committed
event. Intake must not branch on concrete event variants to perform domain
work, mutate semantic state, route operations, synthesize outcomes, or select a
different publication path.

The committed event stream is the boundary between publication and
harness-owned domain processing. Harness consumers react to committed live
events, update projections, perform requested work, and publish resulting
canonical facts or outcomes as later events. Consumers that perform side
effects must not repeat them for replay deliveries. A downstream consumer
cannot veto, rewrite, or erase the event that triggered it.

## Commit and source envelope

Commit means that the central publication path has completed interception,
assigned the runtime sequence and timestamp, written the debug record, completed
any declaratively selected semantic-store append, and accepted the event for
broadcast. A required store failure aborts commit: the event is neither
broadcast nor passed to downstream domain consumers.

There is no new catch-all durable event journal. Existing event-schema policy
continues to select agent, session, restore, current-state, or no durable
storage. `Emit.transient` applies uniformly as generic publication metadata;
`Emit` intake must not override it for a concrete event. Peer requests and
reports are transient unless a separately approved contract defines idempotent
durable recovery. A committed transient event has runtime ordering and live
delivery but no cold-restart replay guarantee.

The authenticated publisher is immutable metadata outside the rewriteable
`Event`. The pre-commit publication envelope delivered to interceptors carries
that publisher, and the same publisher enters the committed envelope delivered
to internal downstream consumers and ordinary subscribers. Interception may
replace the event payload but cannot replace its publisher.

Configured extensions and providers use their stable configured instance name
as publisher identity. A run-local connection ID may identify a live UI or
other unconfigured peer, but it is not durable authority. Any peer-authored
event retained for replay must persist a stable publisher identity and restore
it in the replay delivery envelope. Events whose publisher has no stable
identity must remain transient. Canonical derived facts embed stable provenance
in their typed payload when that provenance remains semantic after replay.

The runtime sequence orders a triggering event before derived events in one
harness process. Restart ordering exists only within the durable stream that
contains both records; this decision creates no cross-journal total order. A
durable request requiring an outcome across crashes needs a separate idempotent
recovery decision.

## Requests, reports, declarations, and canonical facts

An event authored by a peer describes only what that peer has authority to
assert. When the harness owns acceptance, routing, canonical identity, or
resulting state, the peer publishes a request, offer, or report rather than the
harness-owned fact.

A committed request may lead the harness to publish separate accepted,
rejected, started, completed, or canonical state events. Provider, tool,
action, and shell outputs that require correlation or canonicalization are
reports; they are not canonical harness facts merely because they arrived
inside `Emit`. Declarations and externally observed facts need not be renamed
as requests when their producer owns their truth and no pre-publication domain
decision is required.

Pre-publication admission may authenticate the connection, enforce structural
and bounded-resource constraints, attach the immutable publisher envelope, and
reject event names that the peer kind has no authority to author. The authority
matrix is declarative admission policy, not an event-specific operation
handler. Admission may not perform the event's requested operation or make an
offered payload appear to be an accepted harness-owned fact.

Peer event authorship is default-deny. “Configured extension” below means a
harness-authenticated configured instance whose declared client kind and
capability match the row; a `Hello` kind claim alone grants no authority.
External socket peers have no `Emit` authority. Harness-connected core
components have only the entries that explicitly include core.

The following exact authority and wire-name mapping governs the migration:

| Existing peer input | New peer event name | Allowed peer kind(s) | Downstream result |
| --- | --- | --- | --- |
| `tool.register` | `tool.registration_declared` | Configured tool or core extension | Harness publishes canonical `tool.register` or a rejection diagnostic |
| `tool.unregister` | `tool.unregistration_declared` | Configured tool or core extension | Harness publishes canonical `tool.unregister` or a rejection diagnostic |
| `tool.request` | unchanged request name | Configured provider, tool, or core extension | Harness publishes `tool.started` or `tool.rejected` and later terminal projections |
| `tool.progress` | `tool.progress_reported` | Configured tool or core extension | Harness validates routed-call ownership and publishes canonical `tool.progress` |
| `tool.result` | `tool.result_reported` | Configured tool or core extension | Harness validates routed-call ownership and publishes canonical `tool.result` and `provider.tool_result` |
| `tool.error` | `tool.error_reported` | Configured tool or core extension | Harness validates routed-call ownership and publishes canonical `tool.error` and `provider.tool_error` |
| `tool.cancelled` | `tool.cancelled_reported` | Configured tool or core extension | Harness validates routed-call ownership and publishes canonical `tool.cancelled` or background completion |
| `provider.models_updated` | `provider.models_declared` | Configured provider extension | Harness publishes canonical `provider.models_updated` plus availability projections |
| `provider.quota_replace`, `provider.quota_patch`, `provider.quota_clear` | Same suffix under `provider.quota_*_reported` | Configured provider extension | Harness publishes validated `harness.provider_quota_changed` state |
| `provider.prompt_submitted`, `provider.response_updated`, `provider.response_finished`, `provider.retry_prompt_result`, `provider.cache_miss_diagnostic` | Same suffix with `_reported` appended | Configured provider extension | Harness validates prompt ownership and publishes canonical provider facts or directed correlated outcomes |
| `action.schema_published` | `action.schema_declared` | Configured action or core extension | Harness publishes canonical `action.schema_published` or a rejection diagnostic |
| `action.result`, `action.error` | `action.result_reported`, `action.error_reported` | Configured action or core extension | Harness validates invocation ownership, sends the correlated outcome, and publishes a canonical fact only when shared observation is intended |
| The six canonical `message.*` facts | Same name with `_reported` appended | Configured extension declaring `PeerCapability::MessageBridge` | Harness stamps provenance and publishes the canonical durable fact |
| `extension.skill_available`, `extension.agents_md_available`, context-provider registration/readiness, agent-context publication, and prompt-fragment publication | Existing names remain declarations, values, or acknowledgements | Configured extension or core component owning the declaration slot | Post-commit consumers update projections and publish derived state where needed |
| `agent.start_request`, `extension.internal_prompt_submit_request` | Existing request names remain | Configured extension or core component | Harness publishes acceptance/rejection and canonical agent or prompt facts |
| `agent.metadata_set`, `agent.metadata_unset` | `agent.metadata_set_request`, `agent.metadata_unset_request` | Configured extension or attached UI | Harness publishes canonical metadata facts or rejection outcomes |
| `shell.command_progress`, `shell.command_finished` | `shell.command_progress_reported`, `shell.command_finished_reported` | Configured shell extension | Harness validates routed-command ownership and publishes canonical progress/completion; transcript injection follows completion commit |
| State-changing `ui.*` commands | Existing request names remain | Attached local UI | Harness performs the request downstream and publishes canonical state/outcome events |
| `ui.prompt_draft`, `ui.focus_changed` | unchanged | Attached local UI | Live-only consumers react after commit |
| `ui.debug_event_stats_request`, `ui.tree_request`, `ui.detach_request` | Dedicated request messages, not events | Attached local UI | Directed result or connection-control behavior |
| `term.osc1337_set_user_var`, `term.bell` | unchanged | Configured extension, attached UI, or core component | Live-only consumers react after commit |
| Custom `extension.event` | unchanged extension-owned name | Configured extension, attached UI, or core component | Ordinary subscribers consume the committed event directly |
| Harness lifecycle, membership, transcript, status, and all other harness-owned facts | No peer event name | None | Only the harness publishes the canonical fact |

The state-changing UI row comprises `ui.prompt_submitted`, `ui.role_select`,
`ui.agent_model_select`, `ui.role_update`, `ui.shell_command`,
`ui.switch_session`, `ui.create_agent`, `ui.navigate_tree`,
`ui.compact_request`, `ui.cancel_prompt`, `ui.retry_prompt`,
`ui.set_agent_navigation_mode`, `ui.recall_queued_prompt`, and
`ui.set_agent_display_name`. Any new peer-emittable event requires an explicit
authority-matrix amendment rather than falling through a permissive category.

The six existing `message.delivered`, `message.edited`, `message.deleted`,
`message.reaction_added`, `message.reaction_removed`, and `message.sent` names
become harness-owned canonical facts. Message bridges publish distinct report
events. After a report commits, the harness stamps the stable configured
extension publisher, selects the target journal, and publishes the canonical
durable message fact through the ordinary harness publication path. Transcript
projection and agent wake occur only after that canonical fact commits. A
claimed `publisher_extension_id` in a report has no authority. The committed
report and canonical fact are intentionally different records; the former is
transient unless a later decision adds report recovery.

## Interception

Interception is publication policy, not domain processing. Interceptors inspect
an uncommitted event and may pass, replace, or drop it, but they must not treat
an intercept request as proof that the event committed. The committed-stream
boundary constrains harness-owned semantic mutation and requested work; it
cannot prevent an independently implemented interceptor from causing its own
premature external side effects.

Interception applies uniformly to peer-emitted events. If an operation cannot
safely tolerate rewrite or drop, the harness publishes an appropriately
protected canonical event after processing, or the protocol uses a dedicated
message. An interceptor replacement must retain the same event name and
publisher, then pass the same declarative structural and authoring-authority
admission as the original. `Emit` must not gain an event-specific interception
bypass.

## Dedicated messages

Not every peer interaction is an event. A dedicated
`HarnessInputMessage`/`HarnessOutputMessage` variant is required when the
interaction fundamentally needs point-to-point request/response semantics,
private or privileged data, synchronous correlation, pre-publication handling,
or another property that cannot be represented honestly as an emitted event
followed by downstream processing.

Such operations must not overload `Emit` as a polymorphic RPC envelope. Their
messages remain outside event interception, publication, subscriptions, and
replay unless their handlers deliberately publish separate outcome events.

## Lifecycle boundary

Extension activation may buffer `Emit` frames until the peer and global startup
barrier permit publication. Buffering, wire ordering, authentication, and
bounded admission are lifecycle concerns and do not authorize event-specific
domain processing inside the `Emit` handler. Capability data needed to decide
whether an extension may activate must use explicit lifecycle messages when it
cannot be handled as an ordinary committed declaration after activation.

## Consequences

A crash may leave a committed request or report without an outcome. This is an
explicit asynchronous boundary, not an incomplete transaction.

Raw requests/reports and canonical outcomes add events and traffic, but make
authority and causality explicit. High-volume or private interactions should
use dedicated messages when broadcasting both records has no architectural
value.

This decision changes the extension interface and event sequencing under
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
The external-message report-to-canonical flow is specified by
[SPEC-external-message-reports-and-facts](SPEC-external-message-reports-and-facts.md).
The tool declaration-to-canonical-state flow is specified by
[SPEC-tool-declarations-and-canonical-state](SPEC-tool-declarations-and-canonical-state.md).
The tool progress report-to-canonical-fact flow is specified by
[SPEC-tool-progress-reports-and-canonical-facts](SPEC-tool-progress-reports-and-canonical-facts.md).
The terminal tool report-to-canonical-outcome flow is specified by
[SPEC-terminal-tool-reports-and-canonical-outcomes](SPEC-terminal-tool-reports-and-canonical-outcomes.md).
