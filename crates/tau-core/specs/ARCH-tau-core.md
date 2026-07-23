# ARCH-tau-core: Tau core state and store boundaries

`AgentStore` owns per-agent semantic trees and `SessionStore` owns session
membership/event streams. Each supports durable journals and selected
process-lifetime memory streams. Durable records advance their on-disk sequence;
memory-only membership does not consume that cursor. Both durable journal
writers reject encoded records larger than the shared 64 MiB reader allocation
limit before opening or mutating the journal.

Agent journals and both ordinary and restore session journals use the same
failure-atomic frame append. A writer captures the exact pre-append EOF, commits
the length prefix and CBOR payload with a data sync, and advances folded state,
sequence, checkpoint, or session metadata only after that commit. A prefix,
payload, or commit-sync failure truncates to the captured EOF and durably syncs
the rollback. If either rollback operation fails, the live store poisons that
journal path and rejects later appends without reopening it. Verification
ownership is documented in
[Durable journal append tests](../../../docs/testing.md#durable-journal-append-tests).

A durable session keeps ephemeral-agent loads and matching unloads in a separate
process-local, independently sequenced overlay. Late same-daemon replay first
validates and folds the durable snapshot, then validates and composes the
overlay. Cached membership never bypasses durable journal validation, and restart
discards the overlay with the corresponding ephemeral transcripts.

Store IDs used as path components share one bounded safe grammar with CLI
minting, metadata listing, lock probes, and cleanup. They exclude path separators,
NUL, and the reserved `.` and `..` names.

Durability mode is governed by
[DECISION-tau-core-semantic-store-durability](DECISION-tau-core-semantic-store-durability.md).
Per-agent listing checkpoints and their journal-authority boundary are governed
by
[DECISION-tau-core-agent-summary-checkpoints](DECISION-tau-core-agent-summary-checkpoints.md).
Current-session roster enrichment is read-only and path-exact: `AgentStore`
reads at most the bounded first record plus an already-loaded or journal-bound checkpoint
display projection. It does not replay the transcript, repair the checkpoint, or
scan the global agents directory.

`ToolRegistry` stores only harness-accepted runtime registration projections keyed by live
connection. Peer declarations and harness-authored canonical lifecycle events
remain protocol/harness concerns under
[SPEC-tool-declarations-and-canonical-state](../../../specs/SPEC-tool-declarations-and-canonical-state.md);
neither enters core semantic journals.
