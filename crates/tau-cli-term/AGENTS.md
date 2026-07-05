# tau-cli-term instructions

- Read the repository root `AGENTS.md` first.
- For non-trivial CLI UI changes, also read `ARCHITECTURE.md` in this crate.
- Read `design.md` before changing test strategy, bounded subprocess behavior,
  or other durable crate-level decisions.
- Keep bounded subprocess ownership centralized in `src/bounded_command.rs`;
  do not add ad-hoc command timeout/output handling in completion or prompt
  action call sites.
- Terminal foreground ownership belongs only to user-configured commands that
  release raw mode, such as `complete_with_command` and prompt shell/editor
  actions. Git/fuzzy helpers should own a process group for cleanup but must not
  call `tcsetpgrp`.
