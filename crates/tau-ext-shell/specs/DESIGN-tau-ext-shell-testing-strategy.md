# DESIGN-tau-ext-shell-testing-strategy: Testing strategy

Status: inferred

Image-tool tests generate deterministic PNG/JPEG/WebP fixtures, exercise
sniffing, animation rejection, decoder-reported output allocation, the stricter
WebP workspace budget, unchanged high-detail geometry, experimental overview
bounds, EXIF-orientation-before-crop ordering, and crop extent failures. Stable
synthetic geometry fixtures quantify high, overview, and native crop behavior.
They assert foreground-only registration and typed provider content separately
from metadata-only display. A separately gated opt-in real-provider oracle
checks coarse overview and fine high/crop observations; it is evidence required
before changing the default or general image-inspection guidance, not part of
hermetic CI. Its protocol and gates are documented in
[`read_image` visual-fidelity oracle](../../../docs/read-image-fidelity-oracle.md).
Cross-crate provider tests own Responses wire shape, Lite detail omission, route
fail-closed behavior, request-wide raw/data-URL budgets, and digest-preserving
diagnostic data-URL redaction.

## Protocol and schema coverage

Tool registration tests should assert model-visible schemas for every argument
that providers may emit. When an argument is removed from a schema, normal tests
must stop sending it; keep any stale-call compatibility coverage in an explicitly
named legacy test.

Runtime protocol tests should also lock the exact startup subscription set and
the ordering of startup publications before `Ready`, because the extension uses
tau-client helpers for startup and dispatch while preserving a narrow event
exposure boundary.

Provider-owned invocation examples are protocol metadata and must be validated
with `tau_core::validate_tool_examples` in registration tests. Custom/freeform
tools are not fully checked by JSON-schema validation, so keep separate semantic
coverage for any custom/freeform examples that are added.


## UI display coverage

Tests for shell, filesystem, and locking tools should assert `ToolUseState` for
both progress and terminal events when user-visible chips or arguments matter.
For shell execution, cover both directory-lock-disabled display (`mode == ""`)
and directory-lock-enabled inferred modes (`ro` / `rw`).


## Shell process lifecycle coverage

Shell execution tests should cover foreground exit, timeout, cancellation,
signal termination, bounded output capture, and output truncation metadata.
Unix-specific process-group behavior should include regression tests for
background or detached descendants that keep stdout/stderr pipes open after the
foreground shell exits or is killed; the shell tool must return after foreground
completion, timeout, or cancellation rather than waiting for pipe EOF. Tests that
depend on Unix-only helpers such as `setsid` should be `#[cfg(unix)]` and may
skip at runtime when the host lacks the required command.

Non-Unix lifecycle coverage should verify the same foreground-exit,
timeout/cancellation, and bounded-drain semantics on a supported non-Unix target,
with Windows as the primary supported platform. Changes to Windows-only process
waiting code should be compiled for a Windows target when practical, because Unix
CI does not type-check `std::os::windows` imports or platform FFI declarations.


## Directory-lock coverage

Directory-lock tests should cover manual lock lifecycle, automatic lock
selection, waiting progress, cancellation, force-unlock recovery, and same-owner
reentry. Shell read-write inference must be tested through the dispatch path and
through `DirLockManager` so stale or missing manual coverage cannot run as
read-write.

When adding or changing directory-lock backends, keep backend-parity coverage for
path ancestry conflicts, path-local FIFO behavior, duplicate manual locks,
same-owner automatic reentry, `acquire_auto_if_manual_covers` fallback,
cancellation cleanup, release/shutdown/disable cleanup, abandoned diagnostics,
and force-unlock behavior. Path-local FIFO coverage should prove both sides of
the fairness rule: unrelated later waiters may bypass earlier blocked waiters,
while later overlapping waiters remain behind earlier overlapping waiters.
Queued same-owner manual requests must recheck duplicate-lock invariants before
granting so they do not create overlapping manual locks for one owner.
Filesystem-backend tests should also cover cross-instance owner identity,
instance-lease reaping, state-dir validation failures, backend reconfiguration
preserving the previous backend on failure, automatic guards surviving backend
disable/reconfiguration until drop, and read-only polling that does not rewrite
the registry.

Filesystem-backend wait tests should cover the adaptive cross-process polling
schedule, liveness-deadline caps, and same-process condition-variable wake/reset
behavior. Same-process release, cancellation, shutdown, and backend-swap
notifications must be predicate-backed so a wake between registry observation and
timed sleep cannot be lost behind the cross-process backoff ceiling.


## Scheduler coverage

Scheduler lifecycle tests should cover bounded admission, queued-call
cancellation, and drop semantics. Dropping the scheduler is the shutdown boundary:
queued work is discarded, worker threads are woken, and already-running work is
joined before the protocol writer is expected to drain and exit.
Long-running search-tool cancellation tests should distinguish early cancellation
from active process/traversal cancellation and cover the cancellation checks used
after `grep` / `find` work has started.


## Discovery coverage

AGENTS.md discovery tests should cover ancestor ordering, `.agents.local`
ordering, size-cap skips, and user-root ordering. User-root coverage must include
`$HOME/.config/agents`, `$HOME/.config/agents.local`, legacy `$HOME/.agents`,
and legacy `$HOME/.agents.local`, with all user roots preceding project roots.
Symlink coverage must prove AGENTS.md candidates and `.agents.local` roots are
followed because AGENTS.md is trusted prompt input.

Skill discovery tests should cover project roots before user roots and XDG user
skill roots before legacy user skill roots. Collision tests must exercise the
`tau-skills` source-precedence path so duplicate user skills from
`$HOME/.config/agents*` beat duplicate legacy `$HOME/.agents*` skills before
modified-time comparison. Symlink coverage must prove skill roots, nested skill
directories, root-level Markdown skill files, and directory-level `SKILL.md`
files are followed, while canonical-directory cycle detection keeps symlink
loops bounded.
