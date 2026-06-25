# tau-cli-term architecture notes

`tau-cli-term` owns high-level prompt behavior: completion rule selection,
prompt shell actions, prompt-history search, and editor integration. The lower
`tau-cli-term-raw` crate owns raw terminal input state, rendering, redraw
suppression, and pause/resume of terminal mode. `tau-cli-picker` is the small
standalone picker UI used by configured commands.

## Subprocess ownership

Bounded subprocess execution lives in `src/bounded_command.rs`.

- Git/fuzzy completion helpers use `ProcessOwnership::ProcessGroup`: Tau bounds
  stdout and elapsed time, kills the process group on overflow, timeout, or
  inherited-pipe failures, but does not hand foreground terminal ownership to the
  child.
- User-configured `complete_with_command` and prompt shell actions use
  `ProcessOwnership::ForegroundProcessGroup`: Tau first releases raw terminal
  mode, starts the command in an owned process group, hands that group the
  controlling terminal foreground pgrp, then restores Tau's pgrp before
  raw-mode/redraw resume. `tcsetpgrp` is guarded against `SIGTTOU`.

## External-editor prompt trailer

Prompt edit actions write the current prompt to `$TAU_PROMPT_PATH` and, when
context exists, append an exact `TAU trailer` marker line followed by read-only
conversation context. On editor exit, only text before an exact marker line is
used as the prompt. If the marker line is deleted, the whole file is treated as
prompt text.

The shared `EditorContext` mutex is consumed by `tau-cli-term` when constructing
the editor file, but `tau-cli` owns choosing which currently viewed/no-agent
conversation context is published into it. When the edited below-marker trailer
differs from the exact trailer Tau generated for that editor session, Tau stores
the edited trailer in memory and renders it below the marker on the next editor
open. Unchanged trailers and deleted marker lines clear that recovery. Recovery
text remains below the marker and is never submitted unless the user manually
moves it above the marker.

## Git completion cache

Git file enumeration caches results per current directory. Positive results are
kept for the process/cwd snapshot. Negative results are short-lived only, so a
transient git failure, missing repository, or output-limit error does not disable
fuzzy git completion until Tau exits.
