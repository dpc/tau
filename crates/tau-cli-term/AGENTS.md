Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-cli-term instructions

- Read the repository root `AGENTS.md` first.
- For non-trivial CLI UI changes, also read `specs/ARCH-tau-cli-term.md` in this crate.
- Read the applicable Linked Specs under `specs/` before changing test strategy, bounded subprocess behavior,
  or other durable crate-level decisions.
- Keep bounded subprocess ownership centralized in `src/bounded_command.rs`;
  do not add ad-hoc command timeout/output handling in completion or prompt
  action call sites.
- Terminal foreground ownership belongs to interactive commands that release raw
  mode, including the built-in agent picker and user-configured prompt/editor
  actions. Noninteractive git and fuzzy completion helpers should own a process
  group for cleanup but must not call `tcsetpgrp`.
