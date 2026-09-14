<p align="center">
  <img src="docs/logo.svg" width="200" alt="tau logo">
</p>


# ([dpc's](#other-agents-named-tau)) Tau coding agent

Tau is a minimal Unix-first coding agent for people who want local control, simple process boundaries, and tooling that fits naturally into a command-line environment.

Tau runs its main components as standalone POSIX processes and connects them over stdio and Unix sockets.

**New here?** Follow [Your first Tau task](docs/getting-started.md) from
installation and authentication through a small reviewed repository change,
then stop and resume the saved session. Read its trust/data summary before the
first model request.

Components include:

* UI
* Harness
* LLM provider integration
* Extensions

This architecture has important benefits:

* Starting a component is just running a process — supervise, sandbox, restart, or swap it for anything else that speaks the protocol.
* Components can be system-provided, which pairs well with technologies like NixOS.
* Components can be sandboxed individually using tools like bubblewrap, Docker, jails, or Landlock, according to their actual needs.
* Components can be implemented in any programming language.
* It avoids bringing in web technology where it does not belong.

## Features

See [FEATURES.md](FEATURES.md) for a tour of the major features.


## Sister-projects

[Patchmark](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az3sP3WnHgo1UfwmfmFM9a5cZSSEZR) is a diff-aware language server for Markdown and plain-text review notes. It pairs nicely with Tau's prompt-editing flow: open your prompt in `$EDITOR`, review diffs with language-server help, and leave precise feedback for agents without leaving your normal editing environment.


## Status: heavy development, daily use

Tau is still under heavy development, but it has been a productive daily driver for its developers for weeks. It already has features and design choices that are stronger than those in popular coding agents, while the Unix-centric UX keeps improving quickly.

Recent overview video: [The State of Tau #1](https://www.youtube.com/watch?v=v2Ez2umV9xA).

[![The State of Tau #1](https://i.ytimg.com/vi/v2Ez2umV9xA/hqdefault.jpg)](https://www.youtube.com/watch?v=v2Ez2umV9xA)

Terminal demo:

[![asciicast](https://asciinema.org/a/1265333.svg)](https://asciinema.org/a/1265333)

## Installing

The canonical [first-task guide](docs/getting-started.md) explains prerequisites,
provider authentication, a bounded first change, review, troubleshooting, and
resume.

### via Nix

Tau exposes a Nix flake and can be started with `nix run github:dpc/tau`.

You can also import it as a flake input.

### via `cargo`

Tau is a Rust project and can be installed directly from Git:

```sh
cargo install --git https://github.com/dpc/tau --package dpc-tau
```

### via other means

Official packaging will come later — request a format or upvote existing requests on [GitHub Discussions](https://github.com/dpc/tau/discussions) to help prioritize.


## Extensions

Tau runs most capabilities as process extensions. Some standard extensions ship
in this repository; other Tau projects are maintained separately. “External”
below means a separate Tau project, not a third-party project.

### Standard extensions

These extensions ship in the Tau workspace:

| Extension | What it provides |
|---|---|
| [Provider backends](crates/tau-ext-provider-builtin/) | Built-in model providers and provider profile management |
| [Shell and filesystem](crates/tau-ext-shell/) | Shell commands, file operations, locking, and image inspection |
| [Utilities](crates/tau-ext-utils/) | Timers, reminders, and papercut reporting |
| [Web search](crates/tau-ext-websearch/) | Generic web search and URL fetching |
| [Notifications](crates/tau-ext-std-notifications/) | Terminal-facing activity notifications and detached notification commands |
| [Rhai](crates/tau-ext-rhai/) | Opt-in trusted local scripting |

### External Tau projects

Tau maintains these extensions in separate repositories. Published projects are
pinned and re-exported by the Tau flake.

| Integration | What it provides | Flake package | Source |
|---|---|---|---|
| PIM | Email and calendar tools with approval-gated writes | `tau-ext-pim` | [Radicle](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az4FCuiVzFns5iTquhsYCntZyVWCqi) |
| Swarm | Live session projection and task coordination with a Tau Swarm peer | `tau-ext-swarm` | [Radicle](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az38my9x3Rmn6VYtDMiLKgjK3tRv8o) |
| Zulip | Bot message bridge with long polling and scoped send/reaction tools | `tau-ext-zulip` | [Radicle](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az2LFTBWK7VpAwC3Bpxohkh91aqXd) |
| Rostra | Relay-only social client with local state, signed writes, and opt-in notifications | `tau-ext-rostra` | [Radicle](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az4LrrRivcgNjJii5wzbjTvA8ttt6o) |
| Slack | Socket Mode text bridge with scoped send/reaction tools and multiple-instance prefixes | `tau-ext-slack` | [Radicle](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az3NJhEtKWCbHPa28wDQSYJ8eEfBjg) |
| Telegram | Bot API text bridge, plus an optional separately supervised gateway | `tau-ext-telegram`, `tau-telegram-gateway` | [Radicle](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az3sPdSePnxtBvP9pTLwUwVgpMU68r) |
| XMPP | Disabled-by-default XMPP messaging integration | `tau-ext-xmpp` | [Radicle](https://radicle.network/nodes/radicle.dpc.pw/rad%3AzpN6uwkd6ok9qRAX5yZaF7w8xzDd) |

For example:

```sh
nix profile install github:dpc/tau#tau-ext-slack
```

Installing a package only puts its executable on `PATH`. Each integration
remains disabled until you enable its `std-*` instance in `harness.yaml`,
provide the service account or identity and Tau-managed secrets, and configure
the allowed senders, routes, and role tool policy. Installation does not create
an account, add credentials, enable an extension, or authorize any route or
tool. See [Configuring extensions](docs/extensions.md) and each standalone
project's README for the full setup.

### Third-party extensions

- [tau-ext-searxng-search](https://github.com/akohlsmith/tau-searxng-search),
  maintained by akohlsmith, provides a standalone `searxng_search` tool that queries
  a configured SearXNG instance over HTTP, with configurable engines and
  categories plus optional local-category prioritization. See the project's
  documentation for installation and configuration.

Third-party extensions are independently maintained. Listing one here is not a
compatibility, security, or maintenance endorsement; review its source and
documentation before granting it access to Tau.


## Configuration

Use `tau init` to generate config files.

Use `tau provider add` to create or replace built-in provider profiles, including ChatGPT/Codex, OpenAI-compatible Chat Completions, and OpenRouter profiles; edit `harness.yaml` for harness-owned roles, defaults, and extension settings.

Before using private repositories, review [Trust and data](docs/trust-and-data.md).
For normal stop/resume behavior and conservative upgrades, see
[Session lifecycle and upgrades](docs/lifecycle-and-upgrades.md).

`wait({"timeout_minutes": N})` silently clamps its activating-input deadline to
the global `harness.yaml` bounds. The defaults trade small waits for fewer model
rounds while preserving immediate input delivery:

```yaml
wait_timeout_minimum_minutes: 1
wait_timeout_maximum_minutes: 1440
```

Both bounds are positive, inclusive whole minutes, and the maximum cannot
exceed 65,535 minutes because wait registrations persist their effective value
as `u16`. The minimum cannot exceed the maximum. Argument-free and exact
background-result waits do not use these bounds.

By default, `tau` starts the harness daemon and the CLI UI.

To explore other entry points, run `tau -h`.


## Contributing & Contact

**Preferred:** [Tau Zulip chat](https://tauofunix.zulipchat.com)

* [Discord server](https://discord.gg/zens2jjA3U)
* [`#support:dpc.pw` Matrix channel](https://matrix.to/#/#support:dpc.pw)
* [Rostra p2p social network profile](https://rostra.me/profile/rse1okfyp4yj75i6riwbz86mpmbgna3f7qr66aj1njceqoigjabegy)
* [GitHub Discussions](https://github.com/dpc/tau/discussions) — questions, ideas, general conversation
* [Security policy](SECURITY.md) — private vulnerability reporting; see [ARCH-tau](specs/ARCH-tau.md) for system architecture
* [I don't want your PRs anymore](https://dpc.pw/posts/i-dont-want-your-prs-anymore/) — I do not accept pull requests


## License

[Mozilla Public License 2.0](LICENSE)

## Other agents named Tau

Because the author is not very original and forgot to do prior research,
and Tau is just such a good name, there are other coding harnesses called "Tau", like:

* https://github.com/tau-agent/tau - CLI, in Rust, probably quite similar
* https://taulepton.com/
* https://github.com/AbdoKnbGit/tau
* https://alexledger.substack.com/p/tau-the-self-modifying-browser-based

Get used to it, I guess. Now that we can all be so productive,
we'll have forks and personal re-implementations of everything,
with conflicting names.

When you want to be specific, you can call this one "dpc's Tau coding agent".


## AI usage disclosure

[I use LLMs when working on my projects.](https://dpc.pw/posts/personal-ai-usage-disclosure/)

Because of its nature, this project is more AI-assisted than most of my other work.
