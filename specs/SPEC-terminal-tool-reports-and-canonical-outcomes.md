# SPEC-terminal-tool-reports-and-canonical-outcomes: Terminal tool outcome flow

## Record justification

Foreground terminal authority spans configured-report admission, interception,
agent-journal persistence, wait and loop accounting, tool scheduling, lifecycle
synthesis, renderer projection, and provider-context replay, so no single local
artifact can own the complete durable-before-runtime contract.

## Scope

Authenticated configured Tool and Core extensions submit transient
`tool.result_reported`, `tool.error_reported`, and
`tool.cancelled_reported` observations for routed calls. These peer-owned
reports use ordinary generic `HarnessInputMessage::Emit` admission,
interception, commit, and broadcast. Provider, Action, UI, socket,
unconfigured, and stale configured generations have no report authority. No
peer may author canonical `tool.result`, `tool.error`, `tool.cancelled`,
`provider.tool_result`, `provider.tool_error`, or background completion facts.

This specification covers terminal result, error, and cancellation reports
only. Tool request routing, progress, registration lifecycle, actions, and
shell command completion remain separate protocol families.

## Downstream validation

The harness evaluates a terminal report only after it commits. It revalidates
the committed interception replacement against the immutable configured
publisher, source connection, Tool/Core kind, and harness-assigned logical
configured-instance identity captured when publication entered the generic
queue. The report must name a currently tracked call routed to that exact live
source. An unknown, completed, non-owned, harness-internal, disconnected, or
replaced source produces no terminal projection and cannot change tool-turn
state.

Interception may drop a report or replace its payload under the same event
name. A drop has no downstream effect. A replacement reruns every downstream
validation check. A parked report retains its captured publisher identity; a
disconnect or respawn cannot substitute a new generation before validation.
Pre-Ready reports remain ordinary bounded operational messages, preserving
their complete encoded envelope and global activation ordering.

## Canonical projections and completion

A valid foreground result report publishes protected harness-sourced
`provider.tool_result` and then derives `tool.result` after the provider terminal
commits. The generic result clears typed provider content; the provider
projection retains it and remains the durable transcript fact. A valid
foreground error likewise commits protected harness-sourced
`provider.tool_error` before deriving `tool.error`. A valid foreground
cancellation publishes protected harness-sourced `tool.cancelled`, which is its
own durable completion authority.

Canonical terminal facts are immutable and must-pass through interception.
Interceptors may observe them but cannot rewrite or drop them. The harness
retains existing result/error safety validation, tool-name/type enrichment,
result deduplication, wait completion, failure-loop tracking, agent attribution,
tool-turn completion, and provider projection behavior.

If a call already runs in the background, a valid result report publishes
`tool.background_result`; an error or cancellation report publishes
`tool.background_error`. These protected harness-sourced facts preserve the
existing background wait and notification flow and never emit a second
provider-transcript terminal.

The report commits before terminal processing begins. For a journal-backed,
transcript-owned foreground call, the durable provider terminal is the sole
completion authority. Interception parking remains precommit, and routed-call
tracking, wait and loop settlement, foreground completion or backgrounding,
delegate teardown, and next-inference eligibility remain unchanged until its
semantic append succeeds. Renderer projection begins only after that append and
is not transacted with it. Completed-call tracking then prevents a duplicate
report from repeating canonical projections or cleanup; the duplicate report
itself remains an ordinary committed peer observation.

An authoritative append error caused by physical storage or persisted-journal
integrity failure storage-faults the live harness epoch. Pre-write validation,
encoding, size-limit, identity, and persistence-mode rejection does not latch
the fail-stop. After a storage fault, the harness publishes no renderer
projection, applies no terminal-dependent state, rejects further semantic work,
and retains no exact terminal or continuation for online retry. Reopen or
restart rebuilds from the committed prefix; cold recovery never automatically
resends an uncertain tool, provider, or compaction effect. This is governed by
[DECISION-tool-terminal-publication-transactions](DECISION-tool-terminal-publication-transactions.md).

## Persistence, debug logging, and replay

Reports are explicitly transient and never enter agent or session semantic
history. Canonical `tool.result` and `tool.error` are renderer-facing raw facts
and also remain outside semantic history; their protected
`provider.tool_result` / `provider.tool_error` counterparts retain the existing
durable agent-transcript and replay contract. Canonical `tool.cancelled` and
background completion facts retain their existing terminal semantic
persistence. No report has cold-restart replay behavior, so a report committed
immediately before a crash may have no canonical successor.

The runtime event log retains committed reports and canonical projections.
Ordinary non-ephemeral debug JSONL observes attempted projections before
semantic persistence, so a best-effort row may remain when the semantic store
rejects the event and runtime publication does not occur. Result-report debug
projections retain safe metadata but clear typed provider-image bytes under
[SPEC-typed-image-tool-results](SPEC-typed-image-tool-results.md).
Ephemeral-agent suppression applies to raw inbound reports, committed reports,
and every canonical projection, including duplicate or late reports after live
call tracking and the runtime agent are removed, so terminal payloads for
ephemeral agents do not leak into durable debug JSONL. Replay consumes only the
existing canonical semantic facts and never reruns report validation,
canonicalization, cleanup, waits, or background notifications.

This implements the terminal tool rows of
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).
