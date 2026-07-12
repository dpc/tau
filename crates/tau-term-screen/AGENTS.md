Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# `tau-term-screen` agent notes

Read the repository-root `AGENTS.md` before changing this crate.

Also read the applicable trust-boundary records under `specs/` in this directory before changing this crate, especially
layout, sanitization, screen-cache, cursor-movement, or scrolling-render
behavior.

Keep this crate a synchronous terminal layout/rendering library. Do not add
process management, async runtimes, terminal input handling, persistence,
networking, or policy/resource-limit ownership here; those belong to callers or
higher-level crates.
