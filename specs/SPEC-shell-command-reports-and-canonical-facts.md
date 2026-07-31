# SPEC-shell-command-reports-and-canonical-facts: User-shell report flow

## Record justification

User-shell completion spans `tau-ext-shell` producers, generic peer authority
and interception, private route correlation, canonical UI projection, transcript
injection, extension activation, and semantic/debug persistence. No
component-local record defines that complete ordering and trust boundary.

## Authority and publication

Authenticated configured Tool and Core extensions may submit transient
`shell.command_progress_reported` and `shell.command_finished_reported`
observations. These are the extension kinds that can own a registered generic
shell provider; this contract does not change extension kind assignment.
Provider, Action, UI, socket, unconfigured, and disconnected peers have no
report authority. No peer may author canonical `shell.command_progress` or
`shell.command_finished`.

Reports use ordinary generic `HarnessInputMessage::Emit` admission,
same-name interception, commit, debug recording, and broadcast. Pre-Ready
reports are bounded operational traffic deferred behind the global activation
barrier. The harness captures the configured name, connection, instance id,
kind, and frame-admission session before interception.

## Downstream validation and canonical facts

Only after a report commits does the harness revalidate the exact captured
Tool/Core connection generation. It then requires a pending private route whose
selected provider is that connection. Progress must preserve the
harness-recorded target agent. Completion must preserve session, command,
include-in-context, and target-agent identity. Unknown, stale, completed,
non-owned, disconnected, replaced, or identity-altered reports remain
observable reports but produce no canonical fact and consume no route.

A valid report maps the private provider route id back to the UI lifecycle id
and publishes a distinct harness-sourced canonical fact. Canonical progress
retains the existing interception contract: chunk and stream are mutable while
the mapped command id and target remain harness-owned. Canonical completion is
immutable and must-pass, and consumes exactly one pending route. Duplicate or
late reports cannot complete a reused UI lifecycle.

The UI command id remains reserved until canonical completion commits. Only
after that commit and live broadcast may an accepted `include_in_context`
completion inject its tagged user-shell output into the recorded target
transcript. Harness-authored routing, disconnect, and session-shutdown failures
retain their existing canonical completion behavior and do not gain transcript
injection.

## Persistence and replay

Reports default to `persist=false` and never enter agent, session, or restore semantic
stores for either caller-supplied `persist` value. Canonical progress remains
transient. A canonical completion enters only its non-ephemeral target agent's
journal, and only when `include_in_context=true`; it is a self-contained replay
fact carrying the target, command, context flag, bounded final output, and
exit-or-cancel outcome. Thus cold attach can reconstruct exactly one completed
terminal without replaying the transient request or progress observations.
`include_in_context=false` (`!!`), MemoryOnly targets, and other ephemeral
targets remain non-durable. Runtime event logs and ordinary
debug JSONL show authorized non-ephemeral committed reports before their
canonical successors. Debug classification captures the original private route
beside the immutable publisher envelope and rechecks any replacement route
against harness-minted process-lifetime ephemeral-route tombstones. Unknown
peer-chosen routes retain ordinary debug audit treatment and cannot suppress
their own records. The tombstone set grows with distinct accepted ephemeral
user-shell routes until process exit; it contains opaque harness-generated ids,
not arbitrary peer ids. Ephemeral-agent suppression therefore applies to report
and canonical payloads without trusting peer target fields.

An attaching UI receives replay-marked current-state snapshots for at most 128
pending routes in ascending public-command-id order, within a 64 KiB aggregate
CBOR-encoded `UiShellCommand` payload budget. These snapshot bounds do not cap
or reject live routing. A route exceeding the remaining byte
budget is skipped while later smaller payloads remain eligible. One UI-only
replay notice reports the total omitted count. Omitted routes continue normally,
and their eventual live completion renders as a standalone terminal. The
snapshot carries the public lifecycle id and
canonical request identity, but no accumulated progress or output, so the UI can
render one correlated running row and transition that row when the live
completion arrives. This snapshot is directed UI catch-up, not semantic history;
it does not make `!!`, progress chunks, or ephemeral routes durable.
The omission notice is delivered only when the UI selected both
`ui.shell_command` and `harness.notice`.

This implements only the shell command report row of
[SPEC-peer-event-publication](SPEC-peer-event-publication.md).
It does not change UI command routing, the Action family, Tool-versus-Action/Core
semantics, the general publisher envelope, or any other authority row.
