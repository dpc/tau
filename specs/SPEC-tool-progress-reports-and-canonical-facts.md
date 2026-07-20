# SPEC-tool-progress-reports-and-canonical-facts: Tool progress flow

## Scope

Authenticated configured Tool and Core extensions submit transient
`tool.progress_reported` observations for in-flight calls. These peer-owned
reports use ordinary generic `HarnessInputMessage::Emit` admission,
interception, commit, and broadcast. Provider, Action, UI, socket, and
unconfigured peers have no report authority. No peer may author canonical
`tool.progress`.

This specification covers progress only. Tool requests, terminal result/error
reports, cancellation, registration lifecycle, action events, and shell command
events remain separate protocol families.

## Downstream validation and canonical facts

The harness evaluates a progress report only after it commits. It revalidates
the committed interception replacement against the immutable configured
publisher, source connection, Tool/Core kind, and harness-assigned logical
configured-instance identity captured when publication entered the generic
queue. The report must name a currently tracked call routed to that exact live
source. Progress for an unknown, completed, non-owned, harness-internal, or
backgrounded call produces no canonical fact.

A valid report causes the harness to publish a separate protected, transient
`tool.progress` fact with the committed report payload. The canonical event uses
the harness as delivery source and is immutable and must-pass through
interception. The report remains a distinct committed peer observation and
cannot itself update routes or tool-turn state.

Interception may drop a report or replace its payload under the same event name.
A drop has no downstream effect. A replacement reruns every downstream
validation check. If a source disconnects or is replaced while interception is
parked, its report may still commit with the captured source envelope, but it
cannot publish canonical progress or affect the replacement generation.

## Lifetime

Reports and canonical progress facts are live process-lifetime observations.
They do not enter agent or session semantic history and have no cold-restart
replay contract. A report committed before a crash may therefore have no
canonical successor.

This implements the tool progress row of
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).
