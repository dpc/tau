Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-cli-picker instructions

Read `specs/ARCH-tau-cli-picker.md` and the applicable trust-boundary records under `specs/` before changing this crate.

Preserve the crate's synchronous single-select scope and explicit terminal ownership contracts. Do not add embedded TUI behavior, async event loops, or background redraw machinery without first redesigning the public API around host-owned terminal events and sizing.
