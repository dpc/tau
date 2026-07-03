## Workspace layout

- `crates/tau` — main end-user binary that bundles first-party components
- `crates/tau-actions` — shared extension action schema, validation, and slash-line parsing helpers
- `crates/tau-blocking-notify-channel` — blocking notification channel utility
- `crates/tau-client` — shared client/extension protocol runtime API
- `crates/tau-cli` — CLI entrypoint: starts harness daemon and connects UI clients
- `crates/tau-cli-picker` — shared interactive picker for terminal selection prompts
- `crates/tau-cli-term` — higher-level terminal prompt: slash-command/path completion, menu rendering, `$EDITOR` integration
- `crates/tau-cli-term-raw` — raw terminal rendering/input layer
- `crates/tau-config` — user and project configuration loading
- `crates/tau-core` — event bus, routing, state, sessions, policy, and tool registry
- `crates/tau-ext-std-notifications` — built-in notification extension
- `crates/tau-ext-telegram` — disabled-by-default Telegram text bridge extension
- `crates/tau-ext-shell` — shell- and filesystem-oriented extension
- `crates/tau-ext-rhai` — disabled-by-default trusted local Rhai scripting extension for event hooks, tool registration, and direct shell automation
- `crates/tau-ext-test-dummy` — test-only dummy extension
- `crates/tau-ext-websearch` — built-in generic web search extension (Exa default plus opt-in Parallel.ai tools)
- `crates/tau-ext-xmpp` — disabled-by-default XMPP text bridge extension
- `crates/tau-extension` — extension-side protocol/runtime helpers
- `crates/tau-harness` — harness daemon: extensions, bus, sessions, socket server, harness-owned delegate/wait tools
- `crates/tau-provider` — provider credential/config library plus shared provider runtime utilities (OAuth, storage, stream guards)
- `crates/tau-provider-chat-completions` — OpenAI-compatible Chat Completions backend helpers
- `crates/tau-provider-chatgpt` — ChatGPT/Codex Responses backend helpers, including HTTP/SSE, WebSocket, and pool logic
- `crates/tau-ext-provider-builtin` — built-in provider extension plus `tau provider {add,remove,list}` profile CLI
- `crates/tau-proto` — shared protocol types and CBOR codec helpers
- `crates/tau-skills` — skill discovery/loading support
- `crates/tau-socket` — Unix socket transport glue
- `crates/tau-supervisor` — supervised child-process and stdio transport glue
- `crates/tau-term-screen` — terminal screen layout and styled-cell renderer
- `crates/tau-test-support` — reusable end-to-end test utilities
- `crates/tau-themes` — themed text/style types

## Design docs

- `README.md` — project overview, install, configuration, and contact info
- `design.md` — project-wide major design decisions; MUST read before changing
  project-wide policy, CI gates, or cross-cutting behavior
- `FEATURES.md` — major feature tour; update after editing any new major features
- `SECURITY.md` — project-wide security/reliability context and trust boundaries;
  MUST read before changing or reviewing runtime behavior, persistence,
  extension/harness boundaries, tools, or other security/reliability-sensitive code
- `docs/` — focused design and feature notes
- `crates/*/README.md` — crate-specific documentation where present
- `crates/*/ARCHITECTURE.md` — crate-specific architecture notes where present; MUST read before changing that crate
- `crates/*/AGENTS.md` — crate-specific agent instructions - MUST read these (if exists) before modifing code in a given crate

## Common commands

- `cargo check --workspace --all-targets` or `just check` — check Rust code
- `cargo nextest run` or `just test` — run tests
- `treefmt` or `just format` — format code
- `selfci check --candidate <change-id>` — full local CI verification for an explicit candidate change; WARNING: slow, but independent of working copy state, so it can run safely in parallel, and working copy files can be modified without affecting it; prefer to run in parallel and/or only as a final verification step

## Definition of done

- `selfci check --candidate <change-id>` (CI) passes
- Update `FEATURES.md` after editing any new major features.

## Rules

- This project is still very immuture and backward compatibility is never needed.
- ALWAYS consult `tau-commit` skill before making commits
- When asked to debug existing tau sessions, read `tau-self-knowledge-debugging` skill
- Extension configuration errors must never be silent. Extensions that fail to parse or apply their `Configure.config` MUST send `HarnessInputMessage::ConfigError`; the harness MUST surface those as mandatory `harness.notice` and replay them to late UI subscribers.
