# SPEC-terminal-tool-reports-and-canonical-outcomes: Terminal tool outcome flow

## Record justification

Foreground terminal authority spans configured-report admission, interception,
agent-journal persistence, runtime-only canonical publication, wait and loop
accounting, tool scheduling, lifecycle synthesis, renderer projection, and
provider-context replay, so no single local artifact can own the complete
canonical-before-projection-and-settlement contract.

## Scope

Authenticated configured Tool and Core extensions submit transient
`tool.result_reported`, `tool.error_reported`, and
`tool.cancelled_reported` observations for routed calls. These peer-owned
reports use ordinary generic `HarnessInputMessage::Emit` admission,
interception, commit, and broadcast. Provider, Action, UI, socket,
unconfigured, and stale configured generations have no report authority. No
peer may author canonical `tool.result`, `tool.result_display`, `tool.error`, `tool.cancelled`,
`provider.tool_result`, `provider.tool_error`, or background completion facts.

This specification covers terminal result, error, and cancellation reports,
plus the canonical projection and settlement boundary for harness-synthesized
and internal foreground errors without a transcript owner. Tool request
routing, progress, registration lifecycle, actions, and shell command
completion remain separate protocol families.

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
`provider.tool_result` and then derives transient `tool.result` plus
`tool.result_display` after the provider terminal commits. The generic
non-UI projection retains the raw result while clearing provider content. The UI
projection carries only call identity, tool
identity/type, result kind, generic display state, and originator; the provider
projection retains the full raw result and typed provider content and remains the
durable transcript fact. A valid
foreground error likewise commits protected harness-sourced
`provider.tool_error` before deriving `tool.error`. A valid foreground
cancellation publishes protected harness-sourced `tool.cancelled`, which is its
own durable completion authority.

The same provider-first boundary applies when a foreground error has no open
transcript owner, including loaded-agent-correlated internal calls and truly
uncorrelated peer calls. The canonical `provider.tool_error` crosses ordinary
runtime publication and protected interception before `tool.error` is derived.
It does not acquire a semantic-journal owner or durability. Correlated
wait/failure-loop state, tool accounting, caller cleanup, disconnect batches,
and lifecycle teardown settle only after that canonical runtime commit; each
terminal settles once even if the canonical or derived event was parked.

Canonical terminal facts are immutable and must-pass through interception.
Interceptors may observe them but cannot rewrite or drop them. The harness
retains existing result/error safety validation, tool-name/type enrichment,
result deduplication, wait completion, failure-loop tracking, agent attribution,
tool-turn completion, and provider projection behavior.

Exact-text ext-shell editing retains its internal `replace` name in routed
`tool.started` and extension result/error reports. The harness restores the
provider-visible `edit` name on the corresponding canonical provider terminal,
matching the prompt definition and model call without changing the extension's
routing identity.

If a call already runs in the background, a valid result report publishes
`tool.background_result`; an error or cancellation report publishes
`tool.background_error`. These protected harness-sourced facts preserve the
existing background wait and notification flow and never emit a second
provider-transcript terminal. UI subscribers receive the separate payload-free
`tool.background_result_display` projection after the canonical background fact
commits.

The report commits before terminal processing begins. For a journal-backed,
transcript-owned foreground call, the durable provider terminal is the sole
completion authority. Interception parking remains precommit, and routed-call
tracking, wait and loop settlement, foreground completion or backgrounding,
delegate teardown, and next-inference eligibility remain unchanged until its
semantic append succeeds. Renderer projection begins only after that append and
is not transacted with it. Completed-call tracking then prevents a duplicate
report from repeating canonical projections or cleanup; the duplicate report
itself remains an ordinary committed peer observation.

An authoritative foreground append error rejects that terminal operation before
renderer projection or terminal-dependent state. Clean open, lock, and write
failures remain retryable; an unrestored partial write poisons only its journal.
Background sync failure neither retracts the semantic append nor fail-stops the
live harness epoch. Reopen or restart rebuilds the longest valid prefix; cold
recovery never automatically resends an uncertain tool, provider, or compaction
effect.
The writeback and recovery boundary is governed by
[SPEC-semantic-journal-writeback-durability](SPEC-semantic-journal-writeback-durability.md).

## Persistence, debug logging, and replay

Reports are explicitly transient and never enter agent or session semantic
history. `tool.result`, `tool.result_display`, and `tool.error` remain transient
facts; only the display projection is owned by UI consumers. The protected
`provider.tool_result` / `provider.tool_error` counterparts
retain the durable agent-transcript and replay contract. Canonical
provider terminals without an open transcript owner remain runtime-only and
gain no replay contract. Canonical
`tool.cancelled` and background completion facts retain their existing terminal
semantic persistence. UI replay derives the same result-display DTOs from those
full canonical facts without exposing raw successful output. No report has
cold-restart replay behavior, so a report committed immediately before a crash
may have no canonical successor.

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

Canonical provider and background terminals carry independently allocated
observation IDs. The harness writes a content-free, best-effort terminal
classification that references the exact terminal and records completed,
tool-error, cancellation-request, provider-disconnect, lifecycle-teardown,
restart-repair, or unknown cause. A cancellation request that loses a race to a
natural completion does not classify that completion as cancellation. These
observations never gate terminal publication or runtime cleanup; missing
observations produce incomplete trace evidence rather than inferred cause.

This implements the terminal tool rows of
[SPEC-peer-event-publication](SPEC-peer-event-publication.md).
