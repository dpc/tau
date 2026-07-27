# DECISION-async-debug-event-log-writes: Asynchronous debug event-log writes

Authority: confirmed, 2026-07-23, dpc

## Decision

Eligible, already-redacted and serialized per-session `events.jsonl` lines use
one bounded, process-lifetime FIFO writer shared by every harness instance in
the process. Producers perform immediate nonblocking admission; file locking,
opening, appending, flushing, rollback, and worker shutdown never block harness
event or lifecycle work.

The worker owns all debug-log append handles and uses a separate per-file
interprocess lock for each line. Capacity exhaustion or recoverable I/O failure
may drop individual lines. Uncertain rollback poisons the process-wide writer.
Process exit neither joins the worker nor guarantees that queued work drains.

Debug JSONL is non-authoritative best-effort diagnostics. It provides no crash
or power-loss durability and never calls `fsync`. Authoritative semantic
journals retain their existing synchronous durability, ordering, locking, and
replay behavior.

## Rationale

Large debug files can stall the single harness event loop for seconds.
Diagnostic I/O must not delay prompt display, semantic persistence, event
publication, or shutdown. One bounded process-wide worker preserves useful
FIFO order without allowing a slow filesystem to grow memory or detached
writer-thread counts without limit.

Exact queue limits, failure handling, and persistence behavior are specified by
[SPEC-tau-harness-event-processing](../crates/tau-harness/specs/SPEC-tau-harness-event-processing.md)
and
[SPEC-tau-harness-session-state](../crates/tau-harness/specs/SPEC-tau-harness-session-state.md).
