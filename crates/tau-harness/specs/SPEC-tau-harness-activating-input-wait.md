# SPEC-tau-harness-activating-input-wait: Activating Input Wait

## Record justification

Activating-input waits span tool argument and display handling, event-loop waiter state, canonical input queueing and activation classification, lifecycle cancellation, and cold restore, so no one implementation area can own the complete contract.

## Activating-input waits

`wait({"timeout_minutes":N})` is a runtime-only, target-scoped suspension inside
an existing tool round. `N` must be a positive CBOR integer; zero, negatives,
fractions, and other types are errors. `harness.yaml` silently clamps `N` to
the inclusive `wait_timeout_minimum_minutes` and
`wait_timeout_maximum_minutes` bounds before duration conversion; they default
to one and 1,440 minutes. Both are positive whole-minute values, with a
65,535-minute maximum because persisted wait registrations store the effective
timeout as `u16`. The legacy `any_input` field is explicitly rejected. The
deadline starts when the event loop registers the waiter.
Configuration retains the two raw scalar keys through layering and validation,
then the harness carries one validated `WaitTimeoutBounds` policy to argument
normalization and duration conversion.

It completes when canonical input for that agent has passed its normal
acceptance boundary and is queued with inference activation:
user or extension prompts, internal/timer prompts, peer-agent messages and canonical external-message facts, later model-visible watch notifications, loop-guard pivots,
and unsuppressed activating background-completion prompts. Passive restore or
background notices, progress, replay-only traffic, foreground sibling results,
and input for another agent do not wake it. If its deadline is processed first,
it instead completes normally with `timed_out: true` and warning/`timeout` UI
metadata. After the third consecutive activating-input timeout without a status
report or substantive tool admission, that result also carries one bounded
`advice` string telling the agent to call `status` with `waiting`, finish the
current turn (status alone does not finish it), and rely on a message or
trigger for an event-driven wake. The advisory is one-shot for that no-progress
run, and current Waiting status
suppresses it. It never rejects or shortens a wait. Event-loop processing order
decides races exactly once.

The provider-owned generic tool display shows the effective bound as compact
`Nm` arguments, including the configured upper clamp (1,440 minutes by default).
Argument-free waits retain their
existing empty argument display. Live progress and retained/replayed result or
error blocks preserve the same bounded label. Cancellation retains the same
bounded label in its optional complete-replacement generic display descriptor.
The label is static metadata, not a countdown or
elapsed/remaining-time update.

Queueing and waiter registration both execute on the harness event-loop thread.
Starting a wait first checks the same per-agent trigger-ready activation predicate;
otherwise it registers one waiter. Canonical queueing first stores the input, but
only its selected runtime deadline removes that agent's waiter. This level-triggered invariant covers both
queue-before-register and register-before-queue without polling. Wakeup neither
dequeues input nor consumes a background completion, and the content-free result
only promises that input is available.

Input-wait notification is distinct from background-result arbitration. Exact
and bare waits consume a matching already-completed result before its queued
completion prompt can preempt them. Plural exact waits reserve every requested
target and suppress each member's undelivered completion prompt until the whole
set settles or the reservation is released. A different unsuppressed completion
prompt is ordinary activating input and interrupts background waits in either
queue/register order; a wait that consumed or reserved the completion produces no
prompt to wake an input waiter.

Delivery readiness wins equality with an activating-input timeout. Exact tool
waits select wait-tool delay; bare background and activating-input waits select
wait-any delay. An authenticated visible user prompt uses a five-second
wait-tool delay, so completion before that cut coalesces the prompt and expiry
interrupts the wait once with the typed activating-input result.

An interrupted background wait remains a successful scheduling result with the
exact provider-visible headers `tau_internal: true`, `wait_outcome: interrupted`,
`wait_reason: activating_input`, and `wait_mode: exact` or
`wait_mode: exact_all` or `wait_mode: any_background`, followed by exactly one
blank line and optional concise harness-authored prose. Header names and values
are LF-separated closed ASCII tokens. The result contains no activating payload
or redundant target ID. Singular interruption consumes no target completion.
Plural interruption atomically releases every target reservation, consumes no
member, preserves completed-member FIFO order, and restores only undelivered
completion notices that the reservation suppressed. Each notice remains bound to
its exact source `ToolCallRef` and canonical terminal. Reused display IDs do not
coalesce notice obligations: suppression, restoration, and FIFO consumption affect
only the matching generation's queued notice.

## Plural exact waits

`wait({"tool_call_ids":[...]})` selects a distinct transactional wait-all mode.
The array contains one through 64 nonempty valid `ToolCallId` strings, contains
no duplicates, and is mutually exclusive with `tool_call_id` and
`timeout_minutes`. A one-member array retains plural result and durable
correlation shapes. Singular exact, bare background, and activating-input waits
retain their existing interfaces.

The event loop resolves the complete request-order list to exact stable
declaration occurrences before changing runtime state. An unknown or
foreign-owned ID produces the same error. A malformed, duplicate, unknown,
foreign, consumed, foreground-returned, already-reserved, or otherwise
unwaitable member rejects the whole invocation without consuming output,
reserving a member, moving a completion FIFO node, or suppressing a notice.
Successful preflight reserves every member atomically, so later display-ID reuse
cannot retarget the installed set. Exact and plural reservations outrank bare
waits.

Already-completed background members remain retained while reserved. Pending
members may reach their canonical result, error, or cancellation terminal in
foreground or background. A member error never fails fast. The wait settles only
after every member has a canonical terminal and returns this successful native
CBOR envelope in request order:

```text
results:
  - original_tool_call_id: <member ID>
    outcome: result
    output: <original typed CBOR>
  - original_tool_call_id: <member ID>
    outcome: error
    message: <original message>
    details: <original typed details or null>
```

Member payloads remain nested unchanged. Cancellation uses the error member
shape and retains its message and details. Invalid invocation, ownership,
reservation, and wait-operation failures remain tool errors. The runtime consumes
all member outputs atomically only after the wait terminal publishes
successfully. If that terminal append rejects, the harness retains and retries
the exact preallocated terminal; a later terminal for a reused display ID cannot
commit the pending plural settlement or clear the newer generation's notice
ownership.

An unrelated trigger-ready activation interrupts the plural wait; member
completion does not. Event-loop order decides the final-member, activation,
cancellation, and manual-compaction races once. Wait-call cancellation and a
manual-compaction claim preserve set atomicity; rollback restores every
reservation, FIFO position, and only still-undelivered suppressed notice.
Unload, final shutdown, and cold restart drop the runtime reservation. Cold
restart does not recreate the wait and uses ordinary interrupted-tool repair,
while every member terminal remains recoverable by its original owner.

The foreground wait keeps `AgentTurnState::ToolsRunning`; it introduces no
suspended lifecycle state, watcher idle notification, or idle/running watch
edge. Live UI reconnect does not
affect it. Unload, cancellation, or final shutdown drops it, with event-loop
ordering deciding races exactly once. Cold restore does not recreate a waiter:
the unresolved foreground tool follows standard interrupted-tool repair while
durably accepted prompt activation remains available through normal replay.
Agent-message replay reconstructs one payload-free wake for each uncovered
activating durable occurrence. A node-less wake remains dormant behind its exact
inference or tool barrier; checkpoint ancestry suppresses already-covered wakes,
as specified by
[SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md).

UI `:compact` may exclusively claim an activating-input wait only when it is
the target's sole remaining foreground call. The claim removes the deadline
from ordinary arbitration until one canonical cancelled terminal commits or
its append fails. A committed cancellation closes the tool round before manual
compaction starts; queued input remains available afterward. Append failure
restores the original waiter and deadline while retaining the separately
durable queued compaction intent. Cancellation or teardown clears only the
transient wait claim. Repeated requests coalesce while the cancellation is
pending, and event-loop order
decides whether input, timeout, cancellation, or the compaction claim wins.
This locally refines the manual-compaction contract in
[SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md).

## Authorization boundary

Activating-input waits are scheduling, not a new input authority. They wake only
after canonical admitted input is recorded or queued for that exact target
agent, using harness-owned inference-activation classification. Raw extension
traffic, replayed message facts, asserted publisher metadata, and another agent's
input cannot wake them. Both bounded scheduling results disclose no activating
payload: `input_available: true` reports accepted input and `timed_out: true`
reports expiry, with only the optional harness-authored repeated-wait `advice`
described above. External payload, sender, and provenance remain
in ordinary recorded message-fact context and are never rewrapped as
harness-authored tool output.
Wait registration is runtime-only harness state: cancellation, target unload,
and final shutdown remove it, and cold recovery uses ordinary
unresolved-tool repair rather than reviving stale scheduling authority.

The harness also writes content-free, best-effort observations for installed
wait registrations, accepted activation queue items, and wait settlement.
Random observation IDs link an active or immediate outcome to its exact
registration, activation, source terminal, and wait terminal where those facts
survive. Observation append failure never changes registration, wakeup,
timeout, cancellation, teardown, or continuation. Replay never reinstalls or
settles a waiter; omitted facts remain explicitly incomplete or unresolved in
trace projection.
