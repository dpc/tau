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

Agent journals and both ordinary and restore session journals share one bounded,
ordered persistence worker. Live admission validates and advances process-local
folded state and sequence without filesystem I/O. The worker captures the exact
pre-append EOF and writes the complete length prefix and CBOR payload. A prefix
or payload failure truncates to the captured EOF; only inability to restore that
EOF poisons later worker-side appends to that journal.

A lifecycle-owned persistence worker serializes frame and checkpoint writes and
coalesces one dirty state per journal or directory boundary. It syncs complete
frames and typed child-before-parent directory targets in the background and tracks generations
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
[SPEC-provider-prompt-materialization-authority](../../../specs/SPEC-provider-prompt-materialization-authority.md).

`AgentTree` also reconstructs a non-authoritative compaction-chain view from
explicit durable predecessor links, transaction starts and terminals, record
timestamps, and canonical per-attempt accounting. The view reports pass count,
completion state, elapsed-time quality, and known-or-unknown saturating estimated
cost. Corrections replace their awaiting observation. The index is rebuilt by
the same live and cold fold, persists no aggregate, infers no chain membership
from adjacency, and cannot affect recovery, admission, scheduling, or terminal
policy.
See
[SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md)
and
[SPEC-provider-execution-reports-and-canonical-facts](../../../specs/SPEC-provider-execution-reports-and-canonical-facts.md).

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

One Harness-lifecycle `SemanticPersistenceOwner` serves agent, ordinary-session,
and restore journals through one bounded global FIFO. Explicit preparation hands
all mutable files, locks, offsets, checkpoints, manifest authority, and durability
debt to its sole worker. Stores retain generation-bound leases and complete
in-memory projections; live admission reserves count and aggregate bytes, builds
the frame and replacement off-side, then performs one nonblocking revalidation,
swap, and FIFO insertion. Rejection changes no projection or publication state.
The atomic swap drops the superseded complete projection and releases its
staging-only byte reservation immediately; queued work retains only its frame,
incremental projection ownership, checkpoint candidate, and worker/debt state.

The worker retries exact-EOF rollback-safe heads on deadlines, poisons only
generations whose rollback cannot be proven, coalesces watermarked checkpoint and
session-touch debt, and synchronizes file data plus exact child-before-parent
directory targets. Touch debt uses the prepared manifest's `created_at` and an
exact global frame prerequisite. Release closes a complete generation set at one
mutex cut, drains its accepted frames and debt, then drops handles and capacity;
maintenance uses a distinct release/claim/read/finalize lifecycle. Normal
`open`/`open_lazy` constructors are read-only inspection views. The foreground
compatibility writer exists only behind the explicit test-fixture feature. Exact
semantics are governed by
[SPEC-semantic-journal-writeback-durability](../../../specs/SPEC-semantic-journal-writeback-durability.md).

Content-free operational observation reports bounded worker failures and
edge-triggered full, recovered, and drained resource totals. A process-local
wake lets the Harness retry retained runtime publication owners when capacity
returns; it changes neither admission nor asynchronous durability semantics.

Store IDs used as path components share one bounded safe grammar with CLI
minting, metadata listing, lock probes, and cleanup. They exclude path separators,
NUL, and the reserved `.` and `..` names.

A durable session exists when `sessions/<id>/meta.json` commits successfully.
This atomically replaced manifest owns the canonical creation timestamp;
`last_touched` is only a derived ordering and retention hint. Ordinary session
journals remain authoritative for membership and fallback-message facts, and
agent journals remain authoritative for identity and transcript facts. Journal
recovery never reconstructs a missing or invalid canonical session manifest.
Retention treats each surviving canonical session's complete durable
`session.agent_loaded` history as ownership authority for agent journals,
including agents later unloaded. Exact agent deletion evidence comes only from
a checkpoint bound to the current journal EOF plus the journal mtime freshness
fence. Retired agent IDs have permanent sibling tombstones that every durable
mint and creation path treats as reserved.

Memory-only streams use the same semantic fold as journal-backed streams and
support same-daemon replay, but create no durable artifact. Agent journals remain
the sole durable identity and listing authority: atomically replaced `meta.json`
files are versioned, journal-bound derived checkpoints rather than a second
index or evidence of durability. On the persistence worker, a complete journal
frame precedes checkpoint replacement. Missing, stale, corrupt, or over-budget checkpoints
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
