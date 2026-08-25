# SPEC-semantic-journal-writeback-durability: Semantic journal writeback durability

## Record justification

Semantic durability spans core framed storage and persistence workers, harness commit
and external-effect ordering, journal recovery, journal-derived checkpoints, and
lifecycle shutdown, so no one local artifact can own the complete contract.

Normal live paths validate records, reserve bounded nonblocking persistence
capacity, and perform required in-memory ordering and fold work without
filesystem I/O. Saturation or unavailable admission rejects before canonical
acceptance. Accepted jobs enter one ordered, bounded persistence stream;
canonical publication and runtime continuation do not wait for worker-side
open, lock, recovery, write, checkpoint replacement, or synchronization.

The persistence worker writes each complete
`[u64 little-endian length][CBOR payload]` frame in admitted journal order and
atomically replaces journal-derived checkpoints only after their covered frame.
It marks complete writes and newly required directory entries dirty for
coalesced synchronization. Bounded dirty state, generation and offset
watermarks prevent lost wakes and prevent a racing sync from covering later
writes. Storage failures are diagnosed and retried asynchronously without
retracting accepted facts.

A partial-write failure rolls back to the prior EOF. The journal remains usable
after an open, lock, or write failure that leaves no partial frame, or after a
successful rollback; failure to restore the live file poisons that journal. No
semantic durability barrier precedes provider, tool, peer, or other external
effects. Queue accounting includes queued and in-flight work and has no
unbounded fallback. Shutdown may request best-effort persistence or syncing but
must not wait without a bound.

A process, kernel, or power crash may lose recent admitted records, and may lose
or tear the recent written suffix even when an external effect survives. Under
the existing lock, recovery truncates only an incomplete
frame header or payload at EOF, rebuilds affected journal-derived state, and marks that
crash-tail repair dirty. Complete frames that fail decoding, source-shape,
sequence, or semantic validation fail closed byte-for-byte without rebuilding
from a prefix. An empty valid prefix is allowed. Recovery never automatically
resends uncertain external effects. Session manifest existence and creation time
are canonical state, not journal-derived state, and recovery does not reconstruct
them.

Debug `events.jsonl` remains a separate non-authoritative, bounded,
nonblocking, droppable diagnostic stream with no sync or shutdown-drain
guarantee.

The latency and crash-tail choice is governed by
[`GATE-asynchronous-journal-durability`](GATE-asynchronous-journal-durability.md).
The content-free tool-correlation records that use this writeback contract are
specified by
[`SPEC-durable-tool-observation-correlation`](SPEC-durable-tool-observation-correlation.md).
