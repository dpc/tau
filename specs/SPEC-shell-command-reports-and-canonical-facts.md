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
stores for either caller-supplied `persist` value. Canonical progress and
completion retain their existing persistence classifications; this slice does
not change UI replay or transcript history. Runtime event logs and ordinary
debug JSONL show authorized non-ephemeral committed reports before their
canonical successors. Debug classification captures the original private route
beside the immutable publisher envelope and rechecks any replacement route
against harness-minted process-lifetime ephemeral-route tombstones. Unknown
peer-chosen routes retain ordinary debug audit treatment and cannot suppress
their own records. The tombstone set grows with distinct accepted ephemeral
user-shell routes until process exit; it contains opaque harness-generated ids,
not arbitrary peer ids. Ephemeral-agent suppression therefore applies to report
and canonical payloads without trusting peer target fields.

This implements only the shell command report row of
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).
It does not change UI command routing, the Action family, Tool-versus-Action/Core
semantics, the general publisher envelope, or any other authority row.
