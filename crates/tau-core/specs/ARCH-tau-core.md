# ARCH-tau-core: Tau core state and store boundaries

`AgentStore` owns per-agent semantic trees and `SessionStore` owns session
membership/event streams. Each supports durable journals and selected
process-lifetime memory streams. Durable records advance their on-disk sequence;
memory-only membership does not consume that cursor. Both durable journal
writers reject encoded records larger than the shared 64 MiB reader allocation
limit before opening or mutating the journal.

Typed image validation keeps inline history's full media checks. Prepared-image
references receive pure metadata and logical-occurrence quota checks; runtime
admission must verify actual bytes and establish durable protection before
publishing them. Core neither accesses the artifact filesystem nor infers media
safety from a digest. Reference emission is currently disabled.

Startup retention uses a strict `SessionRetentionReferences` projection. It
classifies canonical manifests without following final-component symlinks,
fails closed on manifest or journal I/O uncertainty, streams one bounded record
at a time, and retains only durable loaded-agent IDs plus validated per-session
EOF cursors. Candidate refreshes validate appended suffixes instead of
materializing or replaying complete session histories repeatedly.

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

`AgentTreeIndexes` maps durable context and assistant-response occurrences to
their eventual materialized nodes, including deferred inputs, and records the
exact nodes whose committed source prompt fact carried
`internal_kind=background_tool_completion`. The bit follows the
durable event sequence through both marked-inference and open-tool-round deferred
queues and enters the index atomically with eventual node materialization. Full
journal-prefix replay and snapshots rebuild it without a dispatch-time journal scan;
tree clones carry it with the other replay-derived indexes. Compaction replacement
items never acquire the bit, retained suffix nodes preserve it, and dropping the tree
drops the index, so a stale or foreign `NodeId` cannot confer authority.

The same replay-derived index owns node producing-model provenance from canonical
prompt starts and compaction boundaries. It introduces no journal fields and
never infers origin from backend JSON, usage labels, or the current model.
Destination-specific request projection follows
[REQ-best-effort-provider-switching](../../../specs/REQ-best-effort-provider-switching.md).

A durable session keeps ephemeral-agent loads and matching unloads in a separate
process-local, independently sequenced overlay. Late same-daemon replay first
validates and folds the durable snapshot, then validates and composes the
overlay. Cached membership never bypasses durable journal validation, and restart
discards the overlay with the corresponding ephemeral transcripts.

One Harness-lifecycle `SemanticPersistenceOwner` serves agent, ordinary-session,
and restore journals through one bounded global FIFO. Explicit preparation hands
all mutable journal files, primary locks, offsets, checkpoints, manifest authority,
and durability debt to its sole worker. Stores retain generation-bound leases and
complete in-memory projections. Live admission reserves count and serialization
scratch, measures the frame, then extends the reservation for validation, touched
container growth, new ownership, deferred materialization, and temporary overlap.
It does not copy or size untouched historical payloads. Agent prevalidation owns
the exact record and exclusively borrows its target tree; session admission also
retains exclusive ownership of its records.

After validation and encoding, the producer revalidates the lease and irreversibly
enqueues one frame in a short owner-mutex section. It then synchronously folds
that record into its exclusively borrowed projection outside the owner mutex,
before returning success or allowing publication. Rejection before enqueue changes
no projection or publication state. The worker may persist the frame before the
producer finishes folding; after a crash, replay recovers the same record.
An unexpected fold panic aborts the process so a caller cannot catch unwinding
and access a partially mutated store.

Producer staging bytes remain charged until the fold finishes, independently of
worker frame retirement. Stream release waits for admitted folds as well as
worker I/O. The worker retains only its queued frame, checkpoint candidate, and
worker/debt state. These are conservative requested-allocation bounds, not
allocator RSS bounds. Existing inline-image validation keeps its decoded-output
limit. Third-party codec-internal scratch remains a separate, temporary
limitation; this admission change adds no image decode.

An exact Prepared Agent lease may run a bounded synchronous off-loop storage
closure under the existing agent deletion-exclusion lock. This narrow physical
custody exception does not expose a journal handle, reopen a path, or extend
semantic admission authority. The registry retains only a weak lock reference
and scoped count; acquiring strong custody and dropping its final descriptor
belong off-loop, never under the admission mutex. Closing, poison, fail-stop and
worker exit reject new scopes immediately. An already-running scope may retain
physical exclusion until its I/O unwinds, then drops custody before signalling
drain or returning a completion. Callers bound scopes with their worker admission
permits and revalidate exact lease/runtime ownership before canonical publication.
Guarded work must not wait for the semantic worker, main-loop callbacks, extension
RPC or completion consumption. Agent exclusion precedes the Artifact store lock.
Referenced tool-image and generic standalone replacement admission use this
scope on the artifact worker before canonical publication. Production policy
requires this authority before selecting durable references.

Managed agent projections separate authoritative trees and exact live-query facts
from accepted-record cache residency. The core cache API can evict only a
generation's complete-written prefix, capped by caller-owned suffix pins, and
install a validated off-loop prefix together with the current accepted suffix.
Missing pins or stale read authority fail the handoff without changing live
state. Prepared managed caches never return a partial suffix or silently read
disk. Unprepared agents, including those queried through a managed store, retain
the existing synchronous journal fallback.
The harness owns independent startup, preparation, load-reaction, discovery and
request-suffix protection before using these cache APIs; see
[ARCH-tau-harness](../../tau-harness/specs/ARCH-tau-harness.md).
Eviction changes only residency, not accepted history, journal contents, tree
authority or asynchronous durability guarantees.
Exact live facts retain distinct internal-message texts from submitted, steered
and injected inputs across every accepted branch, so live notice deduplication
does not depend on replay-cache residency. Repeated text shares one fact payload;
distinct internal text grows without a fixed bound with accepted history and is
not covered by the request-suffix budget. Eviction does not reclaim these texts
or the authoritative tree.

The worker retries exact-EOF rollback-safe heads on deadlines, poisons only
generations whose rollback cannot be proven, coalesces watermarked checkpoint and
session-touch debt, and synchronizes file data plus exact child-before-parent
directory targets. Touch debt uses the prepared manifest's `created_at` and an
exact global frame prerequisite. Release closes a complete generation set at one
mutex cut, drains its accepted frames, debt and scoped Agent I/O, then drops
handles and capacity. Finishing queued creation cannot replace Closing with
Prepared. Queueing, debt and guard drain share the release call's original
absolute deadline; an expired command cannot start a fresh guard-wait budget or
claim Released/Ok. After physical handles drop outside the admission mutex, one
final synchronized deadline, availability and generation check publishes the
whole set as Released. A failed or timed-out release is not proof of physical unlock.
The existing bounded owner Drop does not join unrelated storage workers;
maintenance uses a distinct release/claim/read/finalize lifecycle. Normal
`open`/`open_lazy` constructors are read-only inspection views. Test fixtures
write through a fixture-owned `SemanticPersistenceOwner`. Exact semantics are
governed by
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
index or evidence of durability. Standalone metadata from older builds is ignored
for discovery and ID reservation. Later creation may reuse a directory without a
journal, acquiring an existing regular lock when present and leaving unrelated
files untouched. Current checkpoint publication may replace obsolete `meta.json`
contents; it does not create a compatibility archive. On the persistence worker,
a complete journal frame precedes checkpoint replacement. Missing, stale, corrupt,
or over-budget checkpoints must not hide a valid journal-backed agent, and recovery
invalidates a checkpoint when it truncates an incomplete EOF crash tail.

Current-session roster enrichment is read-only and path-exact: `AgentStore`
reads at most the bounded first record plus an already-loaded or journal-bound checkpoint
display projection. It does not replay the transcript, repair the checkpoint, or
scan the global agents directory.

Session membership folding preserves the optional immutable canonical main binding
and rejects replacement or ordinary unload. Agent trees retain typed peer review
metadata and resolve source occurrences by journal identity plus durable sequence,
including hidden branches and deferred inputs. Linked worker receipts rebuild the
release index during live append and cold replay; this index is derived from
receipts, not a second durable consumption record. See
[SPEC-session-main-agent](../../../specs/SPEC-session-main-agent.md) and
[SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md).

`ToolRegistry` stores only harness-accepted runtime registration projections keyed by live
connection. Peer declarations and harness-authored canonical lifecycle events
remain protocol/harness concerns under
[SPEC-tool-declarations-and-canonical-state](../../../specs/SPEC-tool-declarations-and-canonical-state.md);
neither enters core semantic journals.
