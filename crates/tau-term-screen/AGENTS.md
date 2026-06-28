# `tau-term-screen` agent notes

Read the repository-root `AGENTS.md` before changing this crate.

Also read `SECURITY.md` in this directory before changing this crate, especially
layout, sanitization, screen-cache, cursor-movement, or scrolling-render
behavior.

Keep this crate a synchronous terminal layout/rendering library. Do not add
process management, async runtimes, terminal input handling, persistence,
networking, or policy/resource-limit ownership here; those belong to callers or
higher-level crates.
