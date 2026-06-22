# tau-ext-shell testing strategy

## Protocol and schema coverage

Tool registration tests should assert model-visible schemas for every argument
that providers may emit. When an argument is removed from a schema, normal tests
must stop sending it; keep any stale-call compatibility coverage in an explicitly
named legacy test.

Provider-owned invocation examples are protocol metadata and must be validated
with `tau_core::validate_tool_examples` in registration tests. Custom/freeform
tools are not fully checked by JSON-schema validation, so keep separate semantic
coverage for any custom/freeform examples that are added.


## UI display coverage

Tests for shell, filesystem, and locking tools should assert `ToolUseState` for
both progress and terminal events when user-visible chips or arguments matter.
For shell execution, cover both directory-lock-disabled display (`mode == ""`)
and directory-lock-enabled inferred modes (`ro` / `rw`).


## Directory-lock coverage

Directory-lock tests should cover manual lock lifecycle, automatic lock
selection, waiting progress, cancellation, force-unlock recovery, and same-owner
reentry. Shell read-write inference must be tested through the dispatch path and
through `DirLockManager` so stale or missing manual coverage cannot run as
read-write.


## Scheduler coverage

Scheduler lifecycle tests should cover bounded admission, queued-call
cancellation, and drop semantics. Dropping the scheduler is the shutdown boundary:
queued work is discarded, worker threads are woken, and already-running work is
joined before the protocol writer is expected to drain and exit.


## Discovery coverage

AGENTS.md discovery tests should cover ancestor ordering, `.agents.local`
ordering, symlink and size-cap skips, and user-root ordering. User-root coverage
must include `$HOME/.config/agents`, `$HOME/.config/agents.local`, legacy
`$HOME/.agents`, and legacy `$HOME/.agents.local`, with all user roots preceding
project roots.

Skill discovery tests should cover project roots before user roots and XDG user
skill roots before legacy user skill roots. Collision tests must exercise the
`tau-skills` source-precedence path so duplicate user skills from
`$HOME/.config/agents*` beat duplicate legacy `$HOME/.agents*` skills before
modified-time comparison.
