# tau-ext-shell testing strategy

## Protocol and schema coverage

Tool registration tests should assert model-visible schemas for every argument
that providers may emit. When an argument is removed from a schema, normal tests
must stop sending it; keep any stale-call compatibility coverage in an explicitly
named legacy test.


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
