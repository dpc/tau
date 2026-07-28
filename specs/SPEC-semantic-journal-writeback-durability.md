# SPEC-semantic-journal-writeback-durability: Semantic journal writeback durability

## Record justification

Semantic durability spans core framed storage and sync workers, harness commit
and external-effect ordering, journal recovery, derived checkpoints, and
lifecycle shutdown, so no one local artifact can own the complete contract.

Authoritative agent, session, and restore CBOR journals use ordered foreground
frame writes with best-effort coalesced background syncing. The foreground path
writes the complete `[u64 little-endian length][CBOR payload]` frame before
folding or publishing its fact. A semantic append commits after that write and
the fold/publication; storage sync is not part of commit. Open, lock, and write
failures remain synchronous, and semantic records never use an asynchronous or
droppable queue.

Each complete write marks the journal dirty and nonblockingly wakes a
lifecycle-owned sync worker. Journal creation also marks every newly required
directory entry dirty. Dirty state is bounded per journal. Generation and offset
watermarks prevent a racing sync from covering later writes, advance directory
coverage only after required directory syncs succeed, and prevent lost wakes.
Sync failures are logged and retried without retracting facts or blocking later
semantic writes.

A partial-write failure rolls back to the prior EOF. The journal remains usable
after an open, lock, or write failure that leaves no partial frame, or after a
successful rollback; failure to restore the live file poisons that journal. No
semantic durability barrier precedes provider, tool, peer, or other external
effects. Shutdown may request best-effort syncing but must not wait without a
bound.

A process crash normally leaves dirty pages eligible for kernel writeback. A
kernel or power crash may lose or tear the recent suffix even when an external
effect survives. Under the existing lock, recovery truncates only an incomplete
frame header or payload at EOF, rebuilds affected derived state, and marks that
crash-tail repair dirty. Complete frames that fail decoding, source-shape,
sequence, or semantic validation fail closed byte-for-byte without rebuilding
from a prefix. An empty valid prefix is allowed. Recovery never automatically
resends uncertain external effects.

Debug `events.jsonl` remains a separate non-authoritative, bounded,
nonblocking, droppable diagnostic stream with no sync or shutdown-drain
guarantee.

The latency and crash-tail choice is governed by
[`GATE-asynchronous-journal-durability`](GATE-asynchronous-journal-durability.md).
The content-free tool-correlation records that use this writeback contract are
specified by
[`SPEC-durable-tool-observation-correlation`](SPEC-durable-tool-observation-correlation.md).
