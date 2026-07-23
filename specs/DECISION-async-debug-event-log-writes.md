# DECISION-async-debug-event-log-writes: Asynchronous debug event-log writes

Authority: confirmed, 2026-07-23, dpc

## Decision

Every eligible already-redacted and serialized per-session `events.jsonl` line
uses one lazily initialized, process-lifetime FIFO writer worker shared by every
embedded or standalone `Harness` instance in that OS process. Producers attach
by cloning its enqueue handle; dropping a producer never stops or joins the
worker. The singleton and its one global queue prevent sequential or concurrent
harness instances from accumulating detached threads.

Each work item carries its session log path. The worker retains the current open
append handle and changes handles when the FIFO reaches another path.
This includes raw connection/lifecycle observations in their existing
pre-handling position and `published` observations in their existing
pre-semantic-persistence position. The harness attempts immediate, nonblocking
queue admission at that observation point, then event processing continues
without waiting for file lock/open/positioning, write, flush, rollback, or
worker shutdown. Consequently, a `published` debug line may
remain when later semantic persistence rejects the event and no bus broadcast
occurs, as it can today.

The worker exclusively owns every debug-log append handle. Before processing
each line, it acquires the separate worker-only interprocess lock
`<session>/events.jsonl.lock`; this lock is unrelated to and never retains the
authoritative session/agent lock. Waiting for another process's debug worker
therefore blocks only diagnostic work, while semantic session acquisition and
all harness producers continue. The worker holds the debug lock through opening
or selecting the append handle, exact-EOF append, flush, and rollback, then
releases it before waiting idle or processing a different path. It may retain an
open append handle after release, but
every later line reacquires the lock before touching that handle. This per-line
lifetime prevents a detached old producer from interleaving with or truncating a
later producer's line without monopolizing the log while idle.

The global bound covers queued plus in-flight retained work: at most 1,024 lines
and 64 MiB of encoded bytes, including each trailing newline and path metadata.
Serialization may transiently allocate one current line before an oversized
result is rejected. That transient serialized line is not charged to the queue,
and this decision makes no new claim about an event/frame-size bound. A line
whose encoded bytes plus path metadata exceed 64 MiB is rejected. Admission
never waits for capacity. Count or byte overflow drops only the rejected line;
later eligible lines may be admitted when capacity returns. Overflow state is
coalesced and the rejecting producer emits a bounded content-free tracing
warning with the saturated dropped-line count. Already accepted work remains
FIFO.

A file-lock/open error drops that line; a later line for the same path retries.
A returned write or flush error with successful rollback
omits that failed line and continues later accepted lines in order, preserving
the current recoverable failure behavior. These errors use bounded, coalesced,
saturating warning state. If truncation or rollback flush is uncertain, the
existing process-lifetime poison applies to the singleton: the
worker discards the failed and all later queued lines, no producer accepts a
later line for any session, and the worker emits exactly one tracing warning
for the poisoning failure. Worker-side failures are warned directly from the
worker and do not depend on a Harness loop remaining alive; process exit may
still terminate before a pending diagnostic is emitted. Enqueue racing a worker
failure is either rejected from visible poison state or accepted and then
resolved under these same worker rules.

Old-session intake may enqueue its final `session.shutdown` after earlier
eligible lines. New-session work carries its new path and follows accepted
old-session work in the same FIFO. Process shutdown does not stop or join the
process-lifetime worker: it may drain while the process remains alive, but exit
may discard queued work or interrupt in-flight I/O. No harness lifecycle,
provider work, semantic commit, or process exit waits for debug logging.

Debug JSONL never calls `sync_data`, `sync_all`, or Unix `fsync`, and exposes no
option to enable them. The worker preserves the current `File::flush` policy.
After any write or flush failure, rollback truncates to the prior EOF and
flushes that rollback; uncertainty triggers process-lifetime poison. No event or
lifecycle operation waits beyond immediate queue admission.

## Failure and durability boundary

Process or power failure may lose accepted queued lines, tear
the in-flight line, or lose lines whose write and `File::flush` completed but
remain only in operating-system cache. Nonjoining clean process exit has the
same queued/in-flight loss boundary. Restart neither repairs nor salvages a
torn debug tail. Returned I/O failures are failure-atomic only when truncation
and rollback flush succeed.

Debug JSONL is strictly non-authoritative, best-effort diagnostics.
Authoritative CBOR agent/session journals retain their existing synchronous
durability, ordering, recovery, locking, and replay semantics and never use the
debug worker or its lock. Semantic persistence, replay, event broadcast,
lifecycle transitions, provider work, and process exit never await debug worker
I/O. Each JSONL file is an ordered subsequence of eligible debug-log attempts;
overflow, lock/open failure, and recoverably rolled-back I/O failures may omit
individual lines.

## Rationale

Large append-only debug files have caused multi-second filesystem stalls in the
single harness event loop before subscriber broadcast. The debug stream is
useful but non-authoritative, so delaying prompt display or any other harness
work on diagnostic I/O is the wrong dependency direction. A single FIFO worker
retains inspectable order and the current rollback behavior without granting
debug logging authority over semantic commits.

Bounding retained entries and bytes prevents a slow filesystem from growing
memory without limit. Dropping on overflow rather than waiting or disabling
future intake follows the debug-only priority while coalesced accounting avoids
repeated noise. One process-lifetime worker plus its independent per-file lock
avoids an unbounded population of blocked old-session threads without allowing
diagnostic drain to deny later semantic session ownership.

Rotation, retention limits, segmented files, changed JSON schema, and
worker-side serialization are outside this decision.

This decision refines
[SPEC-tau-harness-event-processing](../crates/tau-harness/specs/SPEC-tau-harness-event-processing.md)
and
[SPEC-tau-harness-session-state](../crates/tau-harness/specs/SPEC-tau-harness-session-state.md),
and is governed by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
