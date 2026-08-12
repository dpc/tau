# ARCH-tau-cli-term: tau-cli-term architecture

`tau-cli-term` owns high-level prompt behavior: completion rule selection,
prompt shell actions, prompt-history search, and editor integration. The lower
`tau-cli-term-raw` crate owns raw terminal input state, rendering, redraw
suppression, and pause/resume of terminal mode. `tau-cli-picker` is the small
standalone picker UI used by configured commands.

## Command-mode boundary

`tau-cli-term` recognizes `:` at the first non-whitespace position as Tau's
intrinsic command-mode prefix. This rule is not configurable and takes
precedence over configured completion rules. A doubled prefix (`::`) is a
literal-prompt escape: the high-level terminal canonicalizes it to one leading
colon in prompt history and replaces the raw terminal's last submitted entry
with that canonical spelling. `tau-cli-term-raw` remains syntax-agnostic; it
only exposes the history-replacement operation used by the high-level layer.
Routing and typed literal provenance belong to `tau-cli`, as specified by
[`SPEC-tau-cli-command-mode`](../../tau-cli/specs/SPEC-tau-cli-command-mode.md).

## Subprocess ownership

Bounded subprocess execution lives in `src/bounded_command.rs`.

- Git/fuzzy completion helpers use `ProcessOwnership::ProcessGroup`: Tau bounds
  stdout and elapsed time, kills the process group on overflow, timeout, or
  inherited-pipe failures, but does not hand foreground terminal ownership to the
  child.
- The built-in agent picker starts `fzf` directly with fixed arguments and
  bounded stdin/stdout. It appends one terminal-width-aware, Unicode-width
  presentation field to each stable TSV row, including two-display-column work
  status/current-turn emoji prefix in `<work-emoji><turn-emoji>` order plus the nine-character-wide `$self/$subtree`
  cost pair. It omits the picker-constant live lifecycle and redundant role,
  asks `fzf` to show only the presentation field,
  and removes it from the accepted row. It uses foreground process-group
  ownership and the same raw-mode pause/resume guard as other
  terminal-releasing actions; agent/session interpretation remains in `tau-cli`.
- User-configured `complete_with_command` and prompt shell actions use
  `ProcessOwnership::ForegroundProcessGroup`: Tau first releases raw terminal
  mode, starts the command in an owned process group, hands that group the
  controlling terminal foreground pgrp, then restores Tau's pgrp before
  raw-mode/redraw resume. `tcsetpgrp` is guarded against `SIGTTOU`.
- Bounded command monitoring is event-driven, not poll/sleep based. Captured
  output commands use one monitor channel fed by the direct-child waiter thread,
  stdout reader thread, and optional stdin writer thread. The monitor blocks on
  that channel until the command deadline and treats timeout, stdout overflow,
  stdin failure, and inherited-pipe failure as process-group cleanup paths.
  After the direct child exits, short bounded post-exit waits for stdout/stdin
  pipe closure remain intentional so Tau can detect descendants that inherited
  prompt-owned pipes without waiting indefinitely.
- Inherited-stdio commands use a child-waiter event for timeout monitoring, but
  do not spawn stdout/stdin workers because stdio remains owned by the
  caller/terminal.

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
