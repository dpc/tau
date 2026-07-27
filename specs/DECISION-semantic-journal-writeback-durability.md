# DECISION-semantic-journal-writeback-durability: Semantic journal writeback durability

Authority: confirmed, 2026-07-26, dpc

## Decision

Authoritative agent, session, and restore CBOR journals use ordered foreground
frame writes with best-effort coalesced background syncing. The foreground path
serializes and uses `write_all` to write the complete
`[u64 little-endian length][CBOR payload]` frame to the journal before folding
or publishing its fact, but it neither calls `sync_data` or `sync_all` nor waits
for a durability acknowledgement. A semantic append commits when that complete
foreground frame write succeeds and Tau folds and publishes the fact; storage
sync is not part of commit. Semantic records remain synchronous and ordered; Tau
must not place them in an asynchronous or droppable queue, and open, lock, and
write failures remain synchronous.

After each complete frame write, Tau marks the journal dirty and issues a
nonblocking coalesced wake to a lifecycle-owned sync worker. Creating a journal
also marks its parent directory and every newly created ancestor directory entry
needed to reach it dirty; no such directory is synchronously synced. Dirty state
is bounded to one state per journal, not one job per fact. Generation and offset
watermarks must ensure that a sync racing a later write cannot cover that write
falsely, that directory coverage advances only after every required directory
sync succeeds, and that no dirty wake is lost. Sync failures are logged and
retried; they neither retract published facts nor block or fail later semantic
writes.

A partial-write failure is rolled back to the old EOF without a synchronous
durability barrier. Foreground open, lock, or write failure does not poison the
journal or harness epoch when it leaves no partial frame or rollback restores the
old EOF; later writes may retry. Only failure to restore the live file poisons
that journal.

No semantic-journal durability barrier precedes provider, tool, peer, or other
external effects. A clean shutdown may request best-effort syncing, but it must
not introduce an unbounded harness or event-loop wait.

## Crash and recovery boundary

A process crash normally leaves dirty pages available for kernel writeback. A
kernel or power crash may lose or tear the recent journal suffix even when an
external effect from that suffix survives.

Recovery retains the longest fully framed, decoded, sequence-valid, and
semantically valid prefix. Under the existing journal or store lock and before
later append, Tau truncates the first invalid frame or record and the entire
remaining suffix, including valid-looking later frames. An empty valid prefix is
allowed. Recovery invalidates or rebuilds derived checkpoints and metadata as
needed and marks the repair dirty for background sync.

Debug `events.jsonl` remains a separate, non-authoritative diagnostic stream
under
[DECISION-async-debug-event-log-writes](DECISION-async-debug-event-log-writes.md):
producers admit fully serialized lines nonblockingly to its bounded detached
writer, lines may be dropped, and neither the worker nor shutdown performs a
durability sync or wait.

## Relationship to earlier decisions

This decision supersedes only the contrary durability and recovery clauses in:

- [DECISION-compact-prompt-materialization-authority](DECISION-compact-prompt-materialization-authority.md),
  where provider delivery waits for storage sync;
- [DECISION-tau-core-agent-summary-checkpoints](../crates/tau-core/specs/DECISION-tau-core-agent-summary-checkpoints.md),
  where journal sync precedes checkpoint replacement and recovery fails closed;
- [DECISION-tool-terminal-publication-transactions](DECISION-tool-terminal-publication-transactions.md),
  where any physical-storage or persisted-integrity failure fail-stops the
  journal or harness epoch rather than limiting poison to an unrestored partial
  write; and
- [DECISION-tau-core-semantic-store-durability](../crates/tau-core/specs/DECISION-tau-core-semantic-store-durability.md),
  where replay rejects the whole journal instead of retaining its longest valid
  prefix.

It also replaces the contrast statement in
[DECISION-async-debug-event-log-writes](DECISION-async-debug-event-log-writes.md)
that authoritative journals retain synchronous durability. That record's debug
JSONL policy remains fully in force.

## Rationale

Per-frame storage sync latency currently stalls semantic and event-loop work.
Ordered complete-frame writes preserve immediate write-error reporting and a
recoverable prefix without coupling publication or external effects to
unbounded durability latency. This deliberately accepts duplicate, orphaned, or
inconsistent external consequences after severe crashes instead of adding
cross-system durability barriers.

This decision is governed by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
