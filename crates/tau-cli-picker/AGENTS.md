# tau-cli-picker instructions

Read `ARCHITECTURE.md` and `SECURITY.md` before changing this crate.

Preserve the crate's synchronous single-select scope and explicit terminal ownership contracts. Do not add embedded TUI behavior, async event loops, or background redraw machinery without first redesigning the public API around host-owned terminal events and sizing.
