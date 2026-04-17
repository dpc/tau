<p align="center">
  <img src="docs/logo.svg" width="200" alt="tau logo">
</p>

# Tau coding agent

> Tau is like [Pi][pi], but twice as much.

[Pi][pi] is truly a breath of fresh air in the AI harness space,
but it doesn't go far enough. Tau is twice as as Unix-like,
which is twice as everything.

Instead of being built on top of a Typescript runtime, Tau builds on top
the most venerable, powerful and ubiquitous runtime there is: Unix itself.

Tau runs all its components as standalone Posix processes, communicating
over stdio/RPC.

Components include:

* UI
* harness
* LLM API
* extensions

This architecture has important benefits:

* Starting a component is just running a process with all the power it brings.
* Components can be system-provided, which pairs well with technologies like NixOS.
* Each component can be easily ran on a different VM or system.
  Remote execution is as simple as prefixing a component command with `ssh user@host -c`.
* Each component can be sandboxed individually using tools like bubblewrap, docker, jails, landlock, etc.
  according to its actual needs.
* Components can be implemented in any programming language.
* In theory with a little shim Tau could run [Pi][pi] extensions.
* Avoids bring in web tech where it doesn't belong.

[pi]: https://shittycodingagent.ai/

## Workspace layout

- `crates/tau-proto` — shared protocol types and CBOR codec helpers
- `crates/tau-config` — user and project configuration loading
- `crates/tau-core` — event bus, routing, state, and tool registry
- `crates/tau-supervisor` — supervised child-process and stdio transport glue
- `crates/tau-test-support` — reusable end-to-end test utilities
- `crates/tau-socket` — Unix socket transport glue
- `crates/tau-cli` — CLI entrypoint for embedded and daemon-attached use
- `crates/tau-agent` — first-party agent process
- `crates/tau-ext-fs` — filesystem-oriented extension
- `crates/tau-ext-shell` — shell-oriented extension

## Getting started

- `cargo check`
- `nix develop`
- `selfci check`

## AI usage disclosure

[I use LLMs when working on my projects.](https://dpc.pw/posts/personal-ai-usage-disclosure/),
though due to its nature this project is more "vibed" than I typically would do,
especially in its infancy.
