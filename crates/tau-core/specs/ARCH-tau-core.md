# ARCH-tau-core: Tau core state and store boundaries

`AgentStore` owns per-agent semantic trees and `SessionStore` owns session
membership/event streams. Each supports durable journals and selected
process-lifetime memory streams. Durable records advance their on-disk sequence;
memory-only membership does not consume that cursor. Both durable journal
writers reject encoded records larger than the shared 64 MiB reader allocation
limit before opening or mutating the journal.

`AgentTree` folds `agent.initialization_context_set` as replaceable side state,
not a transcript node. The latest bootstrap/skill replacement survives replay
and remains outside branch-head movement and compaction.

Agent journals and both ordinary and restore session journals use the same
failure-atomic frame append. A writer captures the exact pre-append EOF, writes
the complete length prefix and CBOR payload, then advances folded state and
sequence without waiting for sync. A prefix or payload failure truncates to the
captured EOF. Only inability to restore that EOF poisons the live path.

A lifecycle-owned worker coalesces one dirty state per journal or directory
boundary, syncs complete frames and typed child-before-parent directory targets
in the background, tracks generations
so concurrent writes cannot lose a wake, and retries failures. Sync failure does
not retract or fail an accepted semantic append. Locked recovery truncates only
an incomplete frame header or payload at EOF, then rebuilds folded state and
derived metadata from retained records. Complete framing, decode, source,
sequence, and semantic failures leave the journal unchanged and fail closed.
Verification ownership is documented in
[Durable journal append tests](../../../docs/testing.md#durable-journal-append-tests).
The full crash and external-effect boundary is governed by
[SPEC-semantic-journal-writeback-durability](../../../specs/SPEC-semantic-journal-writeback-durability.md).

Agent journals accept one source-free `agent.prompt_started` only when it uniquely
matches an unresolved durable inference or standalone-compaction owner; they
reject persisted full prompts. This fold and dispatch-authority boundary is
governed by
[SPEC-compact-prompt-materialization-authority](../../../specs/SPEC-compact-prompt-materialization-authority.md).

Prompt facts are the canonical raw-text and typed-provenance authority. Folding
preserves their `PromptSubmissionSource` in derived user-input entries; provider
assembly may then apply the source-specific presentation required by
[SPEC-interactive-user-prompt-envelope](../../../specs/SPEC-interactive-user-prompt-envelope.md).
UI/history/navigation continue to consume canonical facts rather than that late
provider projection.

A durable session keeps ephemeral-agent loads and matching unloads in a separate
process-local, independently sequenced overlay. Late same-daemon replay first
validates and folds the durable snapshot, then validates and composes the
overlay. Cached membership never bypasses durable journal validation, and restart
discards the overlay with the corresponding ephemeral transcripts.

Each durable `AgentStore` and `SessionStore` owns one lazily spawned
`JournalSyncWorker` through its `FramedAppendState`. The worker keeps at most one
merged dirty target and one ready-or-in-flight position per journal or directory
boundary. A journal target records its generation, exact end offset, and required
parent directories. A directory-boundary target records its distinct kind and
child-before-parent directory chain. Newly created directories submit immediately;
after acquiring writable branch ownership, stores deliberately re-cover the
existing branch boundary and a pending store-root chain through `.` or filesystem
root, so a prior process cannot strand any ancestor entry. Opening retains that
root debt without submitting it; read-only use and losing lock contenders neither
submit nor consume it. Foreground appends update their journal target and return
without waiting.

The worker syncs each journal file before its required directories and each
directory boundary child before its parent. It compares the full
kind/generation/offset/directory target before clearing it, and immediately requeues
a concurrently advanced target. Failed paths use independent capped retry
deadlines and rotate fairly behind ready paths. A new dirty notification clears
no existing backoff: later bytes coalesce under the failed path's deadline while
newly dirty paths wake promptly. Thread creation is lazy and best-effort; store destruction
signals one final pass but detaches rather than joining a potentially blocked
filesystem sync. Exact semantics are governed by
[SPEC-semantic-journal-writeback-durability](../../../specs/SPEC-semantic-journal-writeback-durability.md).

Store IDs used as path components share one bounded safe grammar with CLI
minting, metadata listing, lock probes, and cleanup. They exclude path separators,
NUL, and the reserved `.` and `..` names.

A durable session exists when `sessions/<id>/meta.json` commits successfully.
This atomically replaced manifest owns the canonical creation timestamp;
`last_touched` is only a derived ordering and retention hint. Ordinary session
journals remain authoritative for membership and fallback-message facts, and
agent journals remain authoritative for identity and transcript facts. Journal
recovery never reconstructs a missing or invalid canonical session manifest.

Memory-only streams use the same semantic fold as journal-backed streams and
support same-daemon replay, but create no durable artifact. Agent journals remain
the sole durable identity and listing authority: atomically replaced `meta.json`
files are versioned, journal-bound derived checkpoints rather than a second
index or evidence of durability. A complete journal-frame write precedes
checkpoint replacement. Missing, stale, corrupt, or over-budget checkpoints
must not hide a valid journal-backed agent, and recovery invalidates a checkpoint
when it truncates an incomplete EOF crash tail.

Current-session roster enrichment is read-only and path-exact: `AgentStore`
reads at most the bounded first record plus an already-loaded or journal-bound checkpoint
display projection. It does not replay the transcript, repair the checkpoint, or
scan the global agents directory.

`ToolRegistry` stores only harness-accepted runtime registration projections keyed by live
connection. Peer declarations and harness-authored canonical lifecycle events
remain protocol/harness concerns under
[SPEC-tool-declarations-and-canonical-state](../../../specs/SPEC-tool-declarations-and-canonical-state.md);
neither enters core semantic journals.
