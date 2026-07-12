Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-rhai

Before changing this crate, read `specs/ARCH-tau-ext-rhai.md`, the applicable `specs/DESIGN-*.md` records, the applicable trust-boundary records in the repository-root `specs/` directory, and update `README.md` plus `crates/tau-skills/self-knowledge/tau-self-knowledge-ext-rhai.md` when changing script APIs, tool registration/dispatch, shell behavior, or trust boundaries.

Rhai scripts are trusted local code. Do not route `shell_spawn` through `tau-ext-shell` and do not integrate it with ext-shell directory locks.

Keep behavior coverage crate-local where possible: drive protocol frames through `run`, and include shell tests via registered Rhai tools returning `ShellJob`.
