# Features

Tau is a Unix-first coding agent built for local control, durable work, and a
productive terminal workflow. This page is a tour of the major implemented
capabilities, not a complete command or configuration reference. Start with the
[README](README.md) for installation and project philosophy; follow the links
below for details.

## A durable terminal coding workflow

Tau's terminal UI keeps the conversation, tools, and project context together
without surrounding them with heavy chrome. It includes:

- streaming responses, reasoning summaries, inline diffs, and lightweight
  Markdown styling;
- [best-effort persistent prompt history](docs/cli-keybindings.md#prompt-history-persistence),
  path and action completion, configurable key bindings, and prompt editing in
  `$EDITOR`;
- local `!` shell commands, arbitrary picker commands such as `fzf`, and
  user-defined prompt templates;
- commands for agents, sessions, roles, models, transcript branches, and
  display settings; and
- built-in dark and light themes plus user-defined JSON5 themes.

The bottom status includes each selected agent's compact runtime
`$self/$subtree` equivalent-USD API-cost estimate. The subtree follows only
authenticated same-session agent creators, retains completed descendants, and
resets with the active session/runtime. Tau prices each accepted usage record against
the model that served it and uses a documented GPT-5.6 fallback for unpriced
models. It is deliberately an eyeballed comparison, not provider billing
reconciliation.

Sessions can be detached and reattached while work continues. Durable agent
transcripts survive restarts and retain alternate branches when you rewind.
Sessions and agents can also be made ephemeral independently when their state
should exist only for the lifetime of the daemon.

Use `tau attach [SESSION]` for a running target, `tau resume [SESSION]` for
persisted state with an initial UI, and
`tau serve --session SESSION --create|--existing|--create-or-existing` for a
pinned foreground headless owner supervised by another process. The third mode
atomically creates absent state or strictly resumes valid state without repairing
an occupied path. Attach and resume omission opens target selection; serve always
requires an exact ID and one explicit lifecycle mode. See the
[session startup guide](docs/session-startup.md) for identity, locking, and
root-option semantics.

See the [CLI keybinding guide](docs/cli-keybindings.md) for editing and shell
actions, and [`tau-cli`'s architecture](crates/tau-cli/specs/ARCH-tau-cli.md)
for the UI's component boundary.

## Unix-native, replaceable components

The UI, harness, provider backends, and extensions are separate POSIX processes.
They communicate through a typed CBOR protocol over stdio or a local Unix
socket. This makes components independently replaceable, supervisable, and
sandboxable; an extension may even run behind `ssh`, a container command, or
another stdio wrapper.

The `tau` binary bundles the first-party components for convenient daily use,
while configuration still controls which extensions run and which executable
provides each one. Extensions can receive scoped secrets, subscribe to events,
register tools and actions, and—when explicitly authorized—intercept events.
Trusted local automation can also be written with the disabled-by-default Rhai
extension.

For the system shape and ownership boundaries, see
[ARCH-tau](specs/ARCH-tau.md). The [event](docs/events.md),
[message](docs/messages.md), and [interceptor](docs/interceptors.md) references
describe the extension-facing protocol. The
[extension configuration guide](docs/extensions.md) covers enabling, replacing,
and configuring extension processes; the
[Rhai README](crates/tau-ext-rhai/README.md) covers scripting.

## Sessions, event logs, and recovery

Tau records typed session and agent events rather than flattening work into a
single transcript. The harness replays those logs to reconstruct loaded agents,
transcript trees, tool facts, and session membership. Replay is explicitly
marked so stateful subscribers can rebuild their view without repeating
side effects.

Provider execution is streamed into the same event model. Tau reports usage and
context-limit diagnostics, supports model-aware automatic compaction where
available, and offers explicit self- and cross-agent compaction tools when a
role authorizes them. Context recovery is transactional so concurrent facts are
not lost while a transcript is compacted.

See the [event reference](docs/events.md),
[session-state specification](crates/tau-harness/specs/SPEC-tau-harness-session-state.md),
and [compaction and context-recovery specification](specs/SPEC-compaction-and-context-recovery.md).

## Multi-agent work

Agents can delegate a self-contained task to a fresh sub-agent, watch its
progress and responses asynchronously, exchange messages, and collect
background tool results without blocking the main flow. A delegated agent gets
its selected role and tool profile but not the parent's transcript, so the
delegating prompt must include the context it needs.

Messaging works within a session and can use an exact known address across
cooperating local Tau sessions. Cross-session discovery, bare session routing,
and authority to auto-start an entrypoint agent are separately opt-in. Watchers
can see self-reported semantic work status and sanitized provider retry progress without
receiving another agent's hidden prompts or raw provider data. The session-local
watch graph is acyclic; an enable that would close a cycle is rejected without
changing watch state.

Attached UIs share the harness-owned `active`, `active-auto`, or `suspended`
navigation classification for each loaded agent while keeping their selected
transcript, drafts, and presentation local. Explicit classifications survive UI
reconnect while the agent remains loaded in the same daemon session. Successfully
submitting a visible prompt to an existing target implicitly makes it `active`;
an authenticated inter-session message that starts a bare peer-entrypoint
recipient also makes only that new recipient `active`;
selection alone does not. Unload and daemon exit forget both
explicit and implicit writes, and cold restore recomputes defaults.
Running sessions also expose a bounded, pipe-friendly `tau agent list` roster.
`tau session list` similarly prints one line- and ANSI-control-safe row per
distinct session id authoritatively reported by responsive local harnesses,
without treating historical session directories as live. Exact canonical
project-root filtering is available with `--dir`; `--json` returns each
responsive harness's session id and startup project root as one array.
The terminal's C-b binding and `:pick-agent` command pick an effectively active
agent, while `:pick-agent-all` includes all current live agents, through
optional `fzf` without changing durable navigation state. The underlying
actions remain configurable; the all-agent action has no default binding.

See [agent messaging](docs/agent-messaging.md),
[agent roles](docs/agent-roles.md), and the
[agent-watch specification](specs/SPEC-agent-watch.md). The command and picker
format are documented in [Listing and picking session agents](docs/list-agents.md).

## Tools, project context, and policy

Each configured shell extension instance provides everyday filesystem, command,
patch, search, and image-inspection tools and remembers an independent per-agent
workdir. Optional advisory directory locks coordinate mutations by concurrent
agents and can enforce read-only isolation for inferred read-only shell calls.

Tau discovers layered `AGENTS.md` instructions and reusable skills from project,
local, and user configuration roots. Roles can require skills and can shape the
tool surface by exact tool, group, or semantic tag. Capability-conditional
prompt templates keep instructions aligned with the tools and extensions that
are actually available for a turn.

Tool availability is resolved through extension defaults, model compatibility,
policy rules, and the selected role. Socket subscriptions accept only known event
categories; non-socket subscriptions remain unrestricted. The harness injects only
each extension's declared Tau secrets;
configured extensions remain trusted local executables.

Slack, Telegram, and XMPP submit transient `message.*_reported` events through
ordinary interception. After a report commits, the harness stamps the configured
extension publisher, commits an immutable canonical `message.*` fact, and
projects valid facts as untrusted model context. Report submission does not
acknowledge canonical commit; interception, append failure, or a crash may leave
a transport effect without a canonical fact. Transport admission, native routing,
replies, retries, and duplicate suppression remain bridge-local.

See the [skills guide](docs/skills.md), [role guide](docs/agent-roles.md),
[shell process lifecycle](crates/tau-ext-shell/specs/SPEC-tau-ext-shell-process-lifecycle.md),
[directory-locking specification](crates/tau-ext-shell/specs/SPEC-tau-ext-shell-directory-locking.md),
and [external-message architecture](specs/ARCH-external-message-boundary.md).

## Providers and model controls

The bundled provider extension supports ChatGPT/Codex accounts,
OpenAI-compatible Chat Completions endpoints, generic public Responses
endpoints, and OpenRouter profiles. Provider
metadata drives model selection and filters supported reasoning effort, response
verbosity, reasoning summaries, input modalities, and compaction. Provider- and
role-specific behavior also includes service tiers and prompt caching.

Roles package a model, its parameters, prompt fragments, skills, and tool policy
into a reusable agent profile. Controls can be changed for the current process;
model-aware values are filtered or clamped to the model's supported surface. The
UI also surfaces per-turn statistics, cache information, retry state, and
conservative quota-pacing status when the provider supplies enough data.
Provider model current state can carry a runtime-only cache capability/privacy
contract without cache keys, object identities, or lifecycle persistence.
An explicit disabled-by-default harness policy can use safe sliding contracts
for one economical, bounded-wait non-generating refresh; real turns preempt it
and no refresh state survives restart. Generic Chat Completions profiles may reference
externally managed cache objects through non-conflicting request fields, but Tau
does not own their lifecycle or accounting.

The `completions API` route is HTTP/SSE Chat Completions and is suitable for
local servers such as llama.cpp as well as remote compatible services. The
`responses API` route is a separate generic public API-key backend with explicit
`sse` or `websocket` transport, explicit models, and stateless full-transcript
replay. The ChatGPT
OAuth/Codex route is a separate private Responses backend whose inference is
WebSocket-only; it never falls back to HTTP/SSE, though OAuth and quota remain
HTTPS operations. Standalone GPT-5.6 compaction uses a fresh ordinary Responses
WebSocket request with full input and a final compaction trigger, then replaces
that prefix with exactly the validated opaque provider compaction item while
the harness preserves the ordered post-cut suffix. All of these routes share one
startup-snapshotted proxy, `NO_PROXY`, platform-TLS, and optional additive-CA
policy.

See [Providers](docs/providers.md), [Agent roles](docs/agent-roles.md), and the
[provider streaming specification](specs/SPEC-provider-response-streaming.md).

## Personal-information and messaging integrations

First-party extensions connect Tau to services while keeping routing and
authorization explicit:

- **Email and calendars:** `std-pim` offers gated email reading and sending,
  calendar search and free/busy queries, approved calendar mutations, OAuth
  flows, and audit logs. See the
  [PIM README](crates/tau-ext-pim/README.md).
- **Slack:** the disabled-by-default Socket Mode bridge accepts allowlisted
  senders and configured conversations, preserves typed message provenance, and
  limits replies, proactive sends, and separately authorized exact-message
  emoji reactions to authorized routes. See the
  [Slack README](crates/tau-ext-slack/README.md).
- **Telegram:** the disabled-by-default bot bridge connects allowlisted users
  to explicitly registered agents. An experimental local gateway can own one
  bot's polling and route it to multiple Tau sidecars. See the
  [Telegram README](crates/tau-ext-telegram/README.md).
- **Zulip:** the disabled-by-default bot bridge uses native event queues for
  allowlisted direct messages and configured stream/topic routes, with
  source-bound replies and reactions. A separate send-only mode can expose one
  fixed proactive DM without registering any receive queue. See the
  [Zulip README](crates/tau-ext-zulip/README.md).
- **XMPP:** the disabled-by-default bridge supports fixed recipients or
  per-agent MUC rooms with allowlisted senders and TLS transport. See the
  [XMPP README](crates/tau-ext-xmpp/README.md).
- **Web search:** capable ChatGPT/Codex Standard Responses routes use cached
  provider-hosted search by default. Other routes use the bundled extension,
  which rotates search through Exa, Parallel, and anonymous You.com, and fetch
  through Exa and Parallel, with optional
  credentialed Brave, Tavily, and Firecrawl adapters and explicit single-provider
  modes. See the
  [web-search README](crates/tau-ext-websearch/README.md).
- **Notifications:** configurable hooks can emit terminal signals or run
  commands when user-originated turns start or finish, or when one or all loaded
  agents become idle. See the
  [notifications README](crates/tau-ext-std-notifications/README.md).
- **Timers:** the utility extension can schedule one-shot or recurring reminders
  that reactivate the owning agent within the live session. See its
  [architecture note](crates/tau-ext-utils/specs/ARCH-tau-ext-utils.md).

These integrations are ordinary extension processes and can be disabled,
replaced, or configured independently. Tool-producing integrations can use
per-instance tool prefixes and role-level tool policy. Their component READMEs
are the user-facing configuration guides; the adjacent Linked Specs record
architectural boundaries and durable design choices.

## Where to explore next

- [README.md](README.md) — installation, configuration entry points, and project
  status.
- [SECURITY.md](SECURITY.md) — trust boundaries and vulnerability reporting.
- [`docs/`](docs/) — focused guides and protocol references.
- [`crates/*/README.md`](crates/) — component-specific setup and usage where
  available.
- [ARCH-tau](specs/ARCH-tau.md) and scoped
  [`specs/`](specs/) directories — current architecture, specifications, and
  major design decisions.

Tau is under heavy development. For the most exact command-line and
configuration surface, use the installed command's `--help`, generated
configuration from `tau init`, and the component documentation linked above.

Persistent supervised extensions receive the real Tau-state tree recursively
read-only by default. `tau_state_access: hidden` hides unrelated state, and
`legacy` recovers the historical ambient writable view when needed. Memory-only
harnesses always hide Tau state and create no host state.

Persisted activity accounting is available through
`tau session stats --session <id>`. The versioned TOON report traverses immutable
session membership and member agent journals without a cumulative sidecar, and
marks unavailable facts rather than inferring them.

Complete offline durable-agent exports are available through
`tau agent trace <agent-id>`. See [Durable agent trace export](docs/agent-trace.md)
for snapshot guarantees, the canonical native JSONL schema, and the lossy OTLP
visualization adapter. `agent-tools-toon` and `agent-tools-jsonl` provide dense
semantic timelines of provider-declared calls, assistant prose/reasoning,
explicit directional agent messages, activations, and typed causal relationships.
Every item retains journal-relative and qualified absolute append time plus
authoritative journal sequence. Lite retains exact content metrics and at most
4 KiB of each semantic text/output item; full retains complete content.
Completion-delivering waits reference source-owned output without copying it.
Agent trace defaults to TOON lite.
`agent-performance-jsonl` instead emits content-free provider-prompt accounting,
tool/background and typed-wait lifecycle, outer-turn boundaries, standalone
compaction attempts, fixed-point cache ratios, missing-evidence coverage, and
explicitly qualified journal recorded-at wall-clock intervals with one summary
per included agent. Loaded/running agents use a validated, possibly stale
checkpoint prefix without pausing their writer.

Compact trace formats provide a semantic assistant/tool/message timeline in JSONL or TOON. Lite bounds each text/output item to 4 KiB while retaining exact complete-content metrics; full retains complete content. Journal sequence is authoritative within an agent, while cross-agent wall time is only a readable overview and not causality.

## Tau Swarm

The optional `std-swarm` extension publishes a replay-coherent live session
overview to an identity-pinned Tau Swarm peer. It accepts correlated prompts
and blocker answers and provides agent-scoped `task_info`, `task_update`, and
`task_blocker` tools. Command deduplication, replaceable task metadata, blocker
history, updates, and acknowledgements are
process-memory-only and reset on extension restart.
See [Configuring extensions: Tau Swarm](docs/extensions.md#tau-swarm).

Model-facing generic user-payload framing follows [SPEC-exact-sentinel-prompt-envelopes](specs/SPEC-exact-sentinel-prompt-envelopes.md); payload-local XML-like tags do not establish Tau provenance.
