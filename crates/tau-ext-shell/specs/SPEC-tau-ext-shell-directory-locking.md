# SPEC-tau-ext-shell-directory-locking: Directory locking

## Record justification

Directory coordination spans per-agent cwd state, tool admission and path
selection, manual and automatic lock scheduling, memory and filesystem backends,
lifecycle cleanup, and UI recovery, so no single implementation area can own the
complete contract coherently.

`tau-ext-shell` owns advisory directory update coordination for its tools. The
harness does not schedule tools or agent creation under these locks, and each
sub-agent is an independent lock owner.

## Configuration and path authority

Directory locking is opt-in. Enabling `dir_lock` enables its handler and
automatic locking for mutating ext-shell tools. The default memory backend
coordinates one ext-shell process. The filesystem backend coordinates ext-shell
processes for one host user through a private registry and per-instance lease
locks; stale records are reaped only after their lease is no longer held.
Unsafe filesystem-backend initialization fails closed.

Backend reconfiguration initializes the replacement before swapping it in.
Swaps are rejected while automatic locks are active because those guards must
release through the backend that granted them.

The top-level `workdir(path)` setter completes only after the matching committed
per-agent cwd fact. Call-local shell cwd arguments never update that metadata.
Relative filesystem-tool paths are resolved against the admission-time
remembered cwd. The same cwd snapshot determines automatic locks and remains in
force through lock waiting and execution.

## Manual locks and scheduling

`dir_lock(update)` canonicalizes an existing directory and acquires a manual
lock for the calling agent. `dir_lock(unlock)` releases that lock; an explicit
owner may be supplied for recovery. Missing paths and non-directories are
errors.

Locks conflict when either canonical directory contains the other. An owner
cannot acquire a second overlapping manual lock. Same-owner automatic work may
reenter a covering manual lock so tools can mutate without deadlocking.

Granting is path-local FIFO: an active lock blocks overlapping requests, while
an earlier queued waiter blocks only later overlapping waiters. Unrelated work
may proceed. A waiter blocked directly by a manual lock periodically checks
liveness and reports it abandoned after 120 seconds without owner activity or
an active automatic tool.

## Automatic locking

When locking is enabled, mutating ext-shell tools acquire automatic locks before
execution:

- `edit` locks the edited file's resolved parent, or the deepest existing
  ancestor for a create beneath missing parents.
- `apply_patch` locks all source and destination directories as one request.
- shell calls are read-write only when their effective cwd is covered by the
  caller's manual lock; otherwise they run as inferred read-only calls without
  an update lock.

Automatic calls under one covering same-owner manual lock may run concurrently.
Other owners remain blocked until the manual lock is released and its active
automatic calls finish. A queued shell call revalidates covering ownership when
its automatic lock would be granted; stale ownership falls back to read-only
execution.

Read-only filesystem tools and inferred read-only shell calls remain runnable
while update locks are held. Filesystem-backend waiters preserve FIFO and
cancellation across processes through bounded registry rechecks; process-local
release notifications wake local waiters promptly.

## Cleanup, recovery, and UI

Manual locks are released when a tracked delegate or side agent starts, the
owner unloads, or the session ends through shutdown, disconnect, or EOF.
Session cleanup also cancels queued waiters.

The `:shell-dir-force-unlock DIRECTORY` UI action releases all overlapping
manual locks regardless of owner. It neither cancels nor releases automatic
locks held by running tools. Blocked calls publish bounded progress identifying
the directories they await.

## Security boundary

Directory locks coordinate cooperating Tau/ext-shell tools. They are not an
operating-system sandbox or access-control boundary, and tools outside
ext-shell do not participate.

A detached shell descendant may keep mutating after ext-shell exits and its
lease ends. A read-write command may also mutate outside its cwd. Inferred
read-only execution therefore defaults to a native read-only bind mount of the
command cwd and fails closed when that isolation is unavailable. An explicit
configuration opt-out degrades inferred read-only execution to advisory
classification only.

Creates beneath missing parents deliberately lock a broader existing ancestor.
Same-owner reentry can keep other agents waiting for as long as the owner
retains its manual lock.
