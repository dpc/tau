# tau-ext-rhai

Before changing this crate, read `ARCHITECTURE.md`, `design.md`, root `SECURITY.md`, and update `README.md` plus `crates/tau-skills/self-knowledge/tau-self-knowledge-ext-rhai.md` when changing script APIs, tool registration/dispatch, shell behavior, or trust boundaries.

Rhai scripts are trusted local code. Do not route `shell_spawn` through `tau-ext-shell` and do not integrate it with ext-shell directory locks.

Keep behavior coverage crate-local where possible: drive protocol frames through `run`, and include shell tests via registered Rhai tools returning `ShellJob`.
