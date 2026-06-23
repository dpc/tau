# Features

A guide to the major features of (dpc's) Tau coding agent. For high-level
philosophy and motivation see [README.md](README.md); for design notes see
[DESIGN.md](DESIGN.md) and [ARCHITECTURE.md](ARCHITECTURE.md).


## Highlights

- **Unix-native process architecture:** every UI, provider, extension, and tool
  integration is a replaceable process speaking the Tau protocol.
- **Durable agent work:** event logs preserve sessions, transcripts, branch
  trees, rewinds, detach, and resume.
- **Multi-agent workflows:** agents can delegate to isolated sub-agents, exchange
  messages, and collect background tool work without blocking the main flow.
- **PIM extensions:** controlled email and calendar tools expose useful personal
  information while keeping reads, writes, approvals, and logs explicit.
- **Safe shell/filesystem access:** mutating shell and file tools acquire update
  locks so concurrent agents do not trample the same working tree, and each
  shell extension instance remembers per-agent cwd through durable metadata.
- **Trusted Rhai automation:** disabled-by-default local Rhai scripts can subscribe
  to raw events, intercept events, register agent-invokable tools, handle owned
  `tool.started` calls, and use direct async `ShellJob` host shell execution
  outside ext-shell locks.
- **Daily-driver terminal UX:** slash commands, role/model controls, prompt
  history, fzf insertion, editor integration, Markdown-lite transcript styling,
  diffs, thinking blocks, and status telemetry are built into the terminal UI.

### Terminal slash commands and theming

The terminal UI includes local slash commands for session, agent, role/model,
and display control. `/theme <name>` switches the theme for the currently
attached CLI UI process only; it does not write `cli.yaml`, update persisted UI
state, or affect other attached UIs. Completion for `/theme` lists built-in
selectors `tau-plain-dark`, `tau-plain-light`, and `tau-dpc`, plus valid user
themes from `<config_dir>/themes/*.json5`, with descriptions when theme files
provide them. Running `/theme` without an argument prints the same available
theme names and descriptions.

### Markdown-lite transcript styling

The terminal transcript applies lightweight Markdown-like formatting to submitted
prompts, assistant responses, and thinking blocks. Supported syntax includes ATX
headings, list markers, `*strong*` / `**strong**`, `_emphasis_`,
`***strong emphasis***`, `~~strikethrough~~`, inline and fenced code, escaped
markers, and leading-pipe tables with bounded display-only padding.


## Architecture

### Process-oriented components

Every major component — UI, harness, LLM provider, extensions — runs as a
standalone POSIX process and talks CBOR-encoded events over stdio (extensions)
or a Unix socket (UI ↔ harness). A component is just an executable: supervise
it with your init system, sandbox it with bubblewrap or Landlock, swap it for
anything else that speaks the protocol, or write a new one in any language.

The default `tau` binary bundles all first-party components and dispatches via
`tau component <name>` subcommands. Built-in extensions are bundled components
(a subset of all components), and you can replace any extension by editing
`harness.yaml`.

### Persisted event logs

Tau separates session membership from agent transcripts. Session membership
facts are appended to `<state_dir>/sessions/<session_id>/events.cbor`; agent
transcript facts are appended to `<state_dir>/agents/<agent_id>/events.cbor`
(length-prefixed CBOR streams). On resume, the harness folds the session
membership journal, loads the current agents, and replays each loaded agent log
once to rebuild its `AgentTree`. Because agent logs are streams of typed events
rather than flat transcripts, agents branch into trees: rewinding to an earlier
turn keeps the abandoned branch on disk.

```
$ tau session-list
$ tau session-show --session-id <id>
$ tau -r                  # pick a recent session for this cwd
$ tau -r <id>             # resume a specific one
```

`tau --ephemeral` starts a normal live harness session without writing the
session membership journal, session metadata/lock files, debug `events.jsonl`,
per-session harness/extension stderr logs, session-scoped extension data, or
the terminal UI log to disk. This is session-scoped only: agent transcripts
under `<state_dir>/agents/`, provider credentials/state, user/cache extension
data, policy/config files, and runtime sockets keep their normal persistence
behavior. `--ephemeral` is therefore incompatible with `--resume` and
`--attach`, and spawned/loaded agents remain persistent.

Inside the UI, `/tree` prints one-based prompt rewind anchors for the selected
agent. `/tree <anchor>` rewinds to before that prompt, `/tree 0` or `/tree
root` selects the root before the first prompt, and `/tree node <node-id>` is
the explicit expert escape hatch for raw transcript-node navigation.

Any subscriber that joins after a session is initialized — UI clients and
extensions alike — gets the same subscribe-time catch-up: the current session
snapshot plus durable transcript facts, delivered as frames carrying an
explicit `replay` marker so side-effecting consumers (notifications, tool
executors) can skip history while stateful ones fold it. Execution triggers
such as `tool.started` are never replayed. Subscribers already connected when
a resumed session finishes initializing get the same history catch-up at that
moment (their `SessionStarted` arrived live), so a peer's view never depends
on whether it subscribed before or after init.

### Interception system

Components can register as event interceptors with priority + selector pairs;
matching events are routed through the interceptor and only reach the bus and
event log if it allows them. Exact selectors win over prefix matches; ties are
broken by component name. This is how things like the policy gate or the
delegate progress tracker plug in without modifying the harness core.

See [`docs/interceptors.md`](docs/interceptors.md) and
`crates/tau-harness/src/harness/interception.rs`.

### Rhai scripting extension

Tau ships a disabled `std-rhai` extension for trusted local automation. A
configured Rhai script can subscribe to event selectors, handle delivered events,
intercept matching events, and emit JSON-shaped Tau events through host APIs.

During `init`, scripts can imperatively register tool groups and
agent-invokable tools. The extension emits those registrations before `Ready`;
when the harness later delivers a live owned `tool.started` for a registered
name, the Rhai handler receives JSON/CBOR-compatible arguments plus `call_info`
and produces `tool.result` or `tool.error`. Replayed owned starts are ignored so
catch-up history cannot re-run tool side effects.

Trusted scripts can also call `shell_spawn` directly in `tau-ext-rhai`. The call
returns a `ShellJob` and runs asynchronously while the extension continues
processing harness messages. Tool handlers can return a `ShellJob` to defer tool
completion until the shell exits; a completion callback's return value becomes
the deferred result, and callback errors become tool errors. This shell path is
for trusted local host automation and intentionally bypasses `tau-ext-shell` and
its directory-update locks.

The Rust extension owns protocol framing and script failures are reported as
transient `harness.notice` diagnostics or tool errors instead of crashing the
process.

### Remote extensions over SSH

Because extensions are stdio child processes, running one on another machine
is a matter of prefixing its argv with an `ssh` invocation:

```yaml
# harness.yaml
extensions:
  core-shell:
    prefix: ["ssh", "user@host"]
```

The harness prepends `prefix` to the resolved command. Anything that gives you
a stdio pipe to a remote process works the same way (`docker exec`, `nsenter`,
`bwrap`, …).

### Extension secrets

Extensions declare which Tau secrets they may receive in `harness.yaml` under
`extensions.<name>.secrets`. Values are loaded from
`<state_dir>/secrets/<name>.yaml` or one-shot `TAU_SECRET_<NAME>` environment
variables (suffixes are lowercased; use portable names with ASCII letters,
digits, `.`, `_`, and `-`). Environment secrets are removed from the harness
environment after startup and values are sent only to that extension during the
Configure handshake. Secret entries are required by default; set
`optional: true` to omit only that one secret when absent. Set
`extensions.<name>.require: false` when the whole extension is optional and Tau
should continue startup without it if a required declared secret or other
startup/pre-Ready setup fails; the skipped extension is still reported as an
mandatory replayed `harness.notice` notice.
For `std-pim`, migrate old `auth.password_env`, `auth.command`, and
`auth.password_command` settings to `auth.password_secret` plus
`extensions.std-pim.secrets`; the legacy `std-email` alias accepts the same
secret declaration shape.

### Model parameters: effort, verbosity, thinking summary, service tier

Per-prompt knobs are bundled into a single `ModelParams` struct
that the harness stamps onto every `AgentPromptCreated` and that
backends thread through to the provider request:

- **`effort`** — reasoning effort. Six levels (`off`, `minimal`, `low`,
  `medium`, `high`, `xhigh`); maps to provider-specific reasoning controls.
  Provider extensions publish the exact effort levels each model accepts, and
  the harness clamps role/default selections to that published list.
- **`verbosity`** — output verbosity (`low`, `medium`, `high`).
  Sent to providers that advertise support (for ChatGPT/Codex Responses this is
  `text.verbosity`). Default `low` to keep model replies concise. Provider
  extensions publish the accepted verbosity levels per model.
- **`thinking_summary`** — reasoning-summary mode (`off`, `auto`,
  `concise`, `detailed`). Sent as `reasoning.summary` on providers
  that set `supportsReasoningSummary`; ignored otherwise.
- **`service_tier`** — optional upstream service tier. `/fast`
  toggles Codex's `fast` tier. Backends serialize Codex's exact
  OpenAI wire values: `priority` for Fast and `flex` for Flex.

Defaults are normally selected through agent roles in `harness.yaml`:

```yaml
prompt_fragments:
  - name: user.short-plain-style
    priority: 65
    text: Keep answers short and plain, using only simple words.

default_role: senior-engineer
role_groups:
  engineer:
    prompt_fragments:
      - name: engineer.workflow
        priority: 66
        text: Focus on implementation details.
    roles:
      junior-engineer:
        order: 10
        description: Lower-reasoning engineer
        effort: low
      senior-engineer:
        order: 20
        description: Balanced coding engineer
        model: chatgpt/gpt-5.5
        effort: medium
        tools: [read, grep]
        enable_tool_groups: [calendar, email]
        disable_tools: [email_trash]
      staff-engineer:
        order: 30
        description: Maximum-reasoning engineer
        effort: xhigh
      legacy-role:
        enable: false  # hide a lower-layer or built-in role without deleting it
  manager:
    roles:
      micro-manager:
        order: 10
        prompt_fragments:
          - name: manager.workflow
            priority: 66
            text: Delegate non-trivial work.
```

Roles can include an `order` for role cycling within a group and a
`description` shown after the model/knob summary in
`/role ...` completions. Top-level `prompt_fragments` apply to every role in
every role group, including when supplied by one-shot harness config overrides;
group-level fields apply as defaults to that group's roles; per-role
`prompt_fragments` apply only to that role. Roles can set
`compaction` to use provider-default automatic compaction, disable it, or set
an explicit token threshold, and can also use `tools`, `disable_tool_tags`,
`enable_tool_tags`, `disable_tool_groups`, `enable_tool_groups`,
`disable_tools`, and `enable_tools` to customize internal tool availability.
Tools start from extension defaults, matching `tool_policy.rules` apply by tag,
then role overrides run broad-to-specific: disable tags, enable tags, disable
groups, enable groups, disable tools, enable tools. `tools` remains an explicit
role allow-list base when set.

The harness owns model-aware tool-surface policy from provider/tool tags. The
built-in `builtin.chatgpt-shell` rule matches `shell:chatgpt` models, disables
`shell:*`, then re-enables `shell:edit:apply_patch`,
`shell:exec:shell_command`, `shell:cd`, and `shell:lock`. Users can disable or replace that
keyed rule in `tool_policy.rules`; policy rules sort by `priority` (default `0`,
lower first) and then rule name. Tools and models only publish tags.

`default_role` selects the startup role; if it is omitted Tau starts on the
first role in `role_groups` order. Within a group, roles sort by `order` first
and role name second. `tau --role <role>` overrides the startup role
for one newly spawned session. `/model <provider>/<model>` switches the model
for the currently selected agent; `/role <role> <setting> <value>` edits role
settings for the current process only. See
[`docs/agent-roles.md`](docs/agent-roles.md).

In the UI: `/role engineer effort medium`, `/role engineer verbosity low`,
`/role engineer thinking-summary concise`. Tab cycles roles within the current
group; Shift-Tab cycles to the next configured role group.
Model knobs are slash-command-only today. Asking for an unsupported
level (e.g. `effort xhigh` on a mini model, `verbosity high` on a provider
that doesn't support it) degrades and surfaces a `harness.notice` diagnostic rather
than silently dropping the field.

The status bar renders the current session id and selected agent role,
falling back to the model id when no role is selected. Model knobs and
context usage stay out of the bar to keep it compact.

### Prompt input caching

For providers that support it, Tau emits stable `prompt_cache_key` routing
keys (derived from base URL and agent id) so cache hits survive across turns
within an agent transcript, and sets `prompt_cache_retention` where available.
First-party ChatGPT/Codex cache routing is stable for the target agent and does
not change when a turn is direct user input, extension-originated work, a manager
relay, or an agent-to-agent message. Provider compatibility flags live next to
the model entry (`supports_prompt_cache_key`, `supports_prompt_cache_retention`).
Toggle the status-bar hit-rate readout with `/set show-cache-stats <true|false>`.

### Policy / approvals

Subscription approvals are persisted to `<state_dir>/policy.cbor` so that
trusted client/selector pairs don't re-prompt on every reconnect. View them
with `tau policy-show`.


## Built-in extensions

Most built-in integrations are regular extensions under `crates/tau-ext-*/`.
They are configured under `extensions.<name>` in `harness.yaml` and can be
disabled with `enable: false`, marked optional with `require: false`, started
from a configured `cwd:`, swapped via `command:` / `prefix:`, or given free-form
`config:` payload that arrives at startup as a `LifecycleConfigure` message.
Extension names must contain only
ASCII letters, digits, `_`, and `-` so they are safe as state-directory path
components and unambiguous in dotted `--harness-config` paths. Some core tools,
such as `agent_start`,
`wait`, and `skill`, are harness-owned instead of extension processes.

```json5
extensions: {
  "core-shell":         { enable: false },                       // disable
  "std-telegram":       { enable: true, require: false },        // skip visibly if unavailable
  "provider-builtin":   { prefix: ["ssh", "user@host"] },        // run remotely
  "custom-tool":        { command: ["./tool"], cwd: "/srv/tool" }, // run from cwd
  "std-notifications":  { config: { "agent_idle": [{ delay_seconds: 30, osc1337: { key: "user-text-notification", value: "..." } }] } }, // reconfigure
},
```

Repeatable `--harness-config=KEY=VALUE` CLI overrides are applied after config
files for a newly started harness, for example
`tau --harness-config=extensions.core-shell.config.working_directory=/srv/project`.
Values are parsed as YAML, so quote string values that look like booleans,
numbers, `null`, arrays, or maps when you need literal strings. The flag is
rejected for attach-only commands because a running harness cannot apply startup
config overrides after it has already been spawned.

### `core-shell` — shell and filesystem tools

Registers the everyday tools the agent uses to inspect and edit a project:
`shell`, `gpt_shell`, `read`, `edit`, `apply_patch`, `grep`, `find`, `ls`, plus an `echo`
tool for testing. The shell command and any wrapper prefix are configurable:

```json5
"core-shell": {
  config: {
    // Process-wide cwd for ext-shell itself. Useful when core-shell is
    // launched remotely via SSH and the harness-level cwd only affects
    // the local ssh process.
    working_directory: "/srv/project",
    shell: {
      command: "bash",
      prefix: ["nix", "develop", "-c"],
      // User-initiated `!`/`!!` commands are killed after this many
      // seconds. Tool-invoked `shell` calls use their own per-call
      // `timeout` argument (default 120s). Default: 3600 (1 hour).
      user_command_timeout_secs: 3600,
      // Extra env vars injected into `shell` and `!`/`!!` children,
      // applied after the inherited environment so they override or
      // supplement it. Use this to set a custom `PAGER` or adjust paths.
      extra_env: {
        XDG_CONFIG_HOME: "/home/me/.config",
        PAGER: "cat",
      },
    },
    // Advisory directory update locks are disabled by default; set true to opt in.
    // When enabled, shell commands are inferred read-write only while the agent
    // holds a matching manual lock. Otherwise they are inferred read-only.
    // `enforce_ro_bind` defaults true and attempts a read-only bind mount for
    // inferred read-only shell calls.
    dir_lock: { enable: false, enforce_ro_bind: true },
  },
},
```

`working_directory` changes ext-shell's own process cwd after startup config is
received, so default relative paths for shell and filesystem tools resolve there.
It is startup-only: later partial config updates may omit it, but attempts to
change it to a different directory are rejected.

When `dir_lock.enable` is true, the `dir_lock` tool can manually lock an
existing directory for updates, and `edit` / `apply_patch` acquire matching
automatic locks before mutating. `shell` / `gpt_shell` no longer accept an
explicit access-mode argument: while the agent holds a manual lock covering the
shell cwd, the command is inferred read-write and takes an automatic lock;
otherwise it is inferred read-only. With `dir_lock.enable: false`, all shell
commands run as ordinary read-write commands and the UI does not show an access
mode chip. User `!` commands are outside this agent-tool lock path. The
extension also injects `/shell-dir-force-unlock DIRECTORY` so the user can clear
manual locks that overlap a displayed waiting directory.

Tau also discovers project and user agent context from conventional paths. User
`AGENTS.md` files are stacked, not replaced: Tau loads `AGENTS.md` and
`AGENTS.*.md` from `$HOME/.config/agents/`,
`$HOME/.config/agents.local/`, legacy `$HOME/.agents/`, then legacy
`$HOME/.agents.local/`, before current-working-directory ancestors and their
matching `.agents.local/` directories. Skills are loaded from `.agents/skills`
and `.agents.local/skills` under the current working directory, then
`$HOME/.config/agents/skills`, `$HOME/.config/agents.local/skills`, legacy
`$HOME/.agents/skills`, and legacy `$HOME/.agents.local/skills`. Duplicate
user-skill names from XDG roots beat legacy user roots before modified-time
tie-breaking. The `.local` variants are intended for machine- or user-specific
instructions and skills that should usually be added to `.gitignore` instead of
checked in. Skill frontmatter can make skills manual-only with
`disable-model-invocation` and can add `/skill` completion
argument hints with `argument-hint`.

Prompt fragments are composable too: top-level `harness.yaml`
`prompt_fragments` apply to every role in every role group, while
`roles.<name>.prompt_fragments` apply only to that role. Fragments are ordered
by priority with extension- and tool-provided fragments, so global style
instructions, role guidance, and
tool-specific instructions share one prompt assembly path.

Custom prompt templates are separate from system-prompt fragments. Define
`custom_prompts` in `harness.yaml` as a map from prompt id to prompt text, then
type `/prompt <id>` in the CLI to replace the current editable prompt buffer
with that text. The prompt is not submitted automatically, so it can be adjusted
before sending.

```yaml
custom_prompts:
  review: |
    Please review this change carefully.
  summarize: |
    Summarize the current session.
```

Fragment templates also receive `agent_id` when rendering a concrete agent
prompt, plus the durable agent working directory as `cwd` and
`working_directory`, with `eq` and `starts_with` helpers for project-specific
conditionals. `tau dev print-prompt` role previews use a stable fake
`dev-preview-agent` id so the full template is visible, but reusable templates
should still guard agent-specific text with `{{#if agent_id}}`.
`working_directory` contains `present`, `path`, `basename`, and `ancestors`;
`ancestors` is ordered from the working directory up to the filesystem root.

### `std-pim` — PIM (email and calendar) extension

The PIM extension exposes controlled split email and calendar tools for personal
information workflows. Email accounts can list folders with `email_list_folders`,
recent messages by IMAP internal date with `email_list_recent`, read approved or
policy-allowed content with
`email_read`, request full read approval with `email_request_access`, send mail
through approval gates with `email_send`, and safely manage message state with
`email_mark_read`, `email_mark_unread`, `email_star`, `email_unstar`, and
`email_trash`. Message listings include `access=full|preview|none`; pass the row
UID as `email_id` to message-targeting tools. `preview` reads return only a
heavily stripped `body_preview` with HTML removed, links replaced by `LINK`, and
a tiny ASCII character set, while `full` reads return simplified body text
wrapped in `<external_unstrusted_message>`. `/email in deny <id> [id...]`
persists exact read denials as `none` access, but explicit `email_request_access`
calls can ask again. `/email out deny <id> [id...]` persists rejected outgoing
draft approvals as terminal denied ids that cannot later be approved or sent.
`/email in approve`, `/email in deny`, `/email out approve`, and `/email out
deny` accept multiple ids. Agent access and mutation activity is appended as
JSONL and can be reviewed with `/email log last [number]`.
Gmail accounts can use Google-only OAuth2/XOAUTH2 with the broad
`https://mail.google.com/` scope, authorized by `/email auth google start
<account>` and `/email auth google finish <account> <copied-url>` using a
Google Desktop installed-app client and a manually pasted failed loopback
redirect URL, or by a manually supplied refresh-token secret. Google Calendar
keeps its separate TVs/Limited Input device flow. Refresh tokens and pending
PKCE state are private extension state and access tokens are cached in memory
until near expiry.

The same extension also owns split calendar tools and the `/calendar` action
schema. Read-only `ics_feed` accounts can list calendars with
`calendar_list_calendars`, list events with `calendar_search`, read event
details with `calendar_get`, and return free/busy blocks with
`calendar_free_busy` from bounded iCalendar feeds. Google Calendar accounts can
use `/calendar auth google start <account>` and `/calendar auth google finish
<account>` to store OAuth refresh tokens in private extension state, or continue
using manually supplied refresh-token secrets. They support the same
read/free-busy operations plus `calendar_create`, `calendar_update`,
`calendar_delete`, and `calendar_respond` mutations through the
native Calendar API. Google access tokens are cached in memory until near expiry.
Calendar writes are queued for `/calendar change` approval by default, keep ETags
internal for existing events, and use provider conditional requests to avoid
stale overwrites. Calendar reads and write requests are logged to
`logs/calendar.jsonl` and can be reviewed with `/calendar log last [number]`.
Calendar tool results use the same structured `ok`/`command`/`status`/`data`
envelope as email, with `format` fields and `next_cursor` pagination for
line-oriented event rows. Private calendar events default to busy-only model
output. The legacy `std-email` built-in remains as an alias for existing
email-only configs.

### `provider-builtin` — Built-in provider backend

Publishes provider-owned model metadata for ChatGPT/Codex OAuth profiles,
OpenAI-compatible Chat Completions endpoints, and OpenRouter profiles. ChatGPT
publishes hardcoded `chatgpt/*` model metadata from OAuth state; Chat
Completions and OpenRouter profiles publish their configured model lists under
their provider namespaces. The harness assembles prompts, then routes the
selected provider's `agent.prompt_created` event directly to this extension;
there is no built-in `core-agent` process.

Responses conversations chain via `previous_response_id` after the first turn:
each follow-up request sends only the messages added since the prior
`response.id` and lets the upstream API carry reasoning state forward
server-side. The chain is dropped automatically when the selected role resolves
to a different model, on branch edits
(`UiNavigateTree`), and turns that didn't return a `response_id`; if the
upstream rejects the stored id (server-side expiry), the provider falls back to
a full-replay retry once before surfacing the error.

The ChatGPT/Codex surface additionally routes turns over a persistent
**WebSocket** connection. One connection per `(account, agent)` lives in a
small LRU pool inside `provider-builtin`, so the server-side connection-local
response cache stays warm across turns of the same conversation — including
when sub-agent delegations are interleaved with the parent. Connections age out
before the upstream's 60-minute hard cap, and refreshed OAuth tokens invalidate
stale sockets on next use.

### `std-notifications` — idle and turn notifications

Runs configurable `agent_start`, `agent_end`, `agent_idle`, and `agent_idle_all` notification
hook arrays. Hooks can emit OSC 1337 user-vars, terminal bells, and detached
commands with Handlebars-templated arguments. Tau's built-in configuration leaves
notifications disabled; users can opt in to prompt-submit sounds, final-response
sounds, per-agent or all-agents idle desktop notifications after `delay_seconds`
of inactivity, and idle summaries via `agent_summary: true`.

### Harness-owned `agent_start` / `agent_watch` / `wait` / `message` — multi-agent workflows

The harness exposes an `agent_start` tool that spawns an isolated side conversation
and automatically subscribes the caller to its responses, an `agent_watch` tool
that subscribes the calling agent to async notifications when another agent
responds, plus a `wait` tool for collecting background tool results. Long-running background-capable tool
calls return an immediate placeholder, stay visible in the UI, and deliver their
real result or error later so the main turn can keep making progress. Unless the
`agent_start` call supplies `role`, delegated sub-agents default to the
`senior-engineer` role. The `agent_start` placeholder and final result include
`self_agent_id` and `sub_agent_id`; sub-agent responses arrive through distinct
`agent_watch` async response notifications until the caller disables the watch.

When `role` is supplied, or when the default `senior-engineer` role is used, the
sub-agent runs with that role's resolved model, model parameters, system prompt,
and tool profile/filtering. The sub-agent starts with a *fresh* context — only
the parent's `prompt`, the selected role's system prompt, and the selected
role's tools — with no visibility into the parent conversation's prior turns,
tool results, or in-flight state. Inheritable per-agent metadata, such as the shell extension's remembered cwd, is copied to the child so execution context can follow delegation without sharing transcript history. The same isolation applies at every nesting
depth, so sub-sub-agents don't see ancestor task framing and can't be tricked
into re-delegating it. Parent agents are responsible for putting everything the
sub-agent needs into the `prompt`. Live progress (turns, current tool) is shown
in the parent UI alongside the delegate's task name and role. See
[`docs/agent-messaging.md`](docs/agent-messaging.md) for messaging examples.

### `std-telegram` — personal Telegram text bridge

Disabled by default, `std-telegram` lets allowlisted Telegram users send text to
explicitly registered Tau agents and lets those agents reply with
`telegram_send`. It requires a bot-token secret and non-empty `allowed_user_ids`;
outgoing messages use only a configured or linked chat id, never a model-chosen
destination. Runtime registrations and Telegram update offsets are in-memory,
and unconfigured group chats are refused.

### Web search extensions

`std-websearch` proxies web search/fetch tools from one built-in extension. The
Exa-backed `websearch_exa` tool is enabled by default and advertised to models as
`web_search`. Parallel.ai tools are registered in the same extension with
internal names `websearch_parallel_search` / `websearch_parallel_fetch`,
advertised as `web_search` / `web_fetch`, but disabled by default so roles can
opt into them without duplicating the default `web_search` tool. Parallel uses
the default unauthenticated `https://search.parallel.ai/mcp` endpoint; Tau does
not support or send a Parallel API key. `config.parallel_endpoint` can override
the Parallel endpoint.


## CLI / UI

Tau ships a terminal UI that aims for *every pixel of estate is content* —
fast startup, no chrome. The prompt's right side shows the current working
directory, with `$HOME` shortened to `~`.

### Slash commands

Type `/` for menu autocompletion. The built-in set:

| Command             | Effect                                               |
| ------------------- | ---------------------------------------------------- |
| `/quit`             | Exit the session                                     |
| `/detach`           | Leave the UI, keep the harness running for reattach  |
| `/session new`      | Close the current session and start a fresh session |
| `/agent new`        | Clear this UI's selected agent; next untargeted prompt mints a new agent |
| `/ephemeral [on\|off]` | After `/new`, toggle or set whether the next new agent is memory-only |
| `/agent switch <id>` | Switch this UI to a known loaded-agent transcript (`none` clears selection) |
| `/agent suspend [id]` | Hide a loaded agent from this UI's active choices until resumed |
| `/agent resume <id>` | Return a hidden loaded agent to this UI's active choices |
| `/agent name <id> <display name>` | Set a loaded agent's display name |
| `/name <display name>` | Set this UI's currently selected agent display name |
| `/suspend` / `/resume` | Suspend or resume this UI's currently selected agent |
| `/model <provider>/<model>` | Switch selected agent model                         |
| `/role <role> ...`  | Switch, create, edit, or delete an agent role        |
| `/fast`             | Toggle Codex Fast mode (`service_tier: fast`)        |
| `/tree [anchor]`    | Print prompt rewind anchors; with `anchor`, rewind before that prompt |
| `/set <name> <val>` | Set a UI setting (Tab cycles names + values)         |
| `/skill <name> [args]` | Invoke a user-invocable skill; `/skill:<name>` is also accepted |

Unknown leading slash roots are treated as local CLI notices instead of being
submitted as prompt text. This catches mistyped commands like `/modle` early,
while ordinary prompts that contain slashes later in the line are still sent to
the selected agent normally.

A session is an agent-membership container backed by a durable membership log.
Starting a new session resets harness/UI session state; prompts create/load
agents whose transcripts are stored under `<state_dir>/agents/<agent_id>/`. The
"current agent" selection is local to each attached UI: `/agent new`, `/agent
switch`, `/agent suspend`, `/agent resume`, `/agent name`, `/name`, `/suspend`,
and `/resume` do not synchronize selection or hidden-agent preferences to other
UIs.

`/agent switch` completion lists active agents so suspended transcripts stay out
of normal prompt-routing choices. If you explicitly type a known suspended agent
id, `/agent switch <id>` still selects that transcript for viewing; prompts
remain blocked for that selected agent until `/agent resume <id>` or `/resume`
marks it active again.

Available `/set` names include `show-diff` (expanded vs. compact diffs),
`show-thinking` (agent reasoning summaries), `show-turn-stats` (per-turn
token usage below responses), `redraw-counter` (debug redraw counter),
`redraw-history-size` (history lines replayed on full redraw),
`show-ui-io` (UI↔harness socket throughput), and
`show-prompt-scroll-indicator` (hidden-row indicator for capped prompt input).
The boolean settings take `true` / `false`; `redraw-history-size` takes a
non-negative line count and defaults from `cli.yaml`.

Prompt input is capped to `floor(33% of terminal height)` with a minimum of
one editable row. Long drafts scroll inside this prompt-local viewport instead
of taking over the whole screen. Plain `Up` / `Down` keep completion-menu
priority, then move/scroll within the capped input, and only fall through to
prompt history once the input edge is reached. Explicit history shortcuts such
as `C-p` / `C-n` and `C-Up` / `C-Down` bypass local input scrolling. When rows
are hidden and the cap is at least two rows, Tau shows a compact hidden-row
indicator inside the cap; `/set show-prompt-scroll-indicator false` disables
it.

`/set show-messages <none|self-summary|self-full|all-summary|all-full>`
controls how agent-to-agent messages are shown in the transcript; messages sent
from an agent to `user` always render fully as human-visible broadcasts.
`/set notice-level <critical|warning|info|debug|trace>` controls which
harness/UI notices are shown; `info` is the default, while `warning` hides
routine lifecycle chatter but preserves warnings and critical failures. Critical
or mandatory notices such as extension configuration errors remain visible. The
first-arg menu shows the meaning of each allowed value. State is persisted to
`<state_dir>/cli.json`.

On full redraws such as terminal resize, agent switches, or retroactive UI
toggles, the terminal UI clears Tau-owned scrollback and replays only the most
recent rendered history lines up to `redraw_history_size`. Lowering
`/set redraw-history-size <lines>` takes effect on the next full redraw; raising
it forces an immediate full redraw so more history is restored right away.

### Prompt input history

Submitted prompt lines are kept in prompt history for the current run and are
also appended to `<state_dir>/prompt-history.cbor`. New `tau` processes seed
Up/Down prompt recall from that file, so recent prompts from previous runs are
available like in-session history.

### Path autocompletion

Prompt word completions are configured in `~/.config/tau/cli.yaml` under
`completions`. Built-ins map `@` to `complete_agents`, `./`, `../`, `/`, `~`,
and `~/` to plain `complete_path`; a `/` that is the first non-whitespace
character always opens slash/action completion for now. Other available
completers are `complete_path_fuzzy`, `complete_actions`, and
`complete_with_command <argv...>`; command arguments are split on whitespace,
run when the trigger token is typed exactly, and replace it with trimmed stdout.

`complete_path_fuzzy` fuzzy-searches git-tracked and unignored files in the
current repository using `nucleo-matcher`; outside a git repository, or when no
fuzzy matches are found, it falls back to directory prefix completion. Plain path
completion reads matching directory entries. `@...` remains reserved for agent
mention completion. Standard fzf-style fuzzy-search bindings are also available
inside the completion menu. Slash-command arguments use the same menu but are
populated dynamically by the harness (model list, effort levels, …).

### Bang shell commands

A prompt line starting with `!` runs a shell command from the UI. `!<cmd>`
renders live stdout/stderr in the transcript and injects the finished output
back into the agent context as a `<user_shell>` block, so the agent can see
what you ran and use the result.

Use `!!<cmd>` for UI-only commands: output is rendered the same way, but is
marked `[no context]` and is not replayed to the agent.

Examples:

```text
!ls
!!git status
```

### Customizable key bindings

`cli.yaml` exposes a `bind:` table that maps key chords to prompt-local
actions. Bindings are layered on top of built-ins; user entries with the
same key replace the built-in binding.

See [`docs/cli-keybindings.md`](docs/cli-keybindings.md) for the built-in key
bindings and the full set of configurable prompt/application actions. Common
actions include:

- `submit-prompt`: submit the prompt, or accept a previewed completion without
  submitting.
- `insert-newline`: insert a newline at the cursor.
- `shell-prompt-edit`: dump the prompt to `$TAU_PROMPT_PATH`, run the shell
  command, then replace the prompt with the file contents on success.
- `shell-prompt-insert`: run the shell command and insert its stdout at the
  cursor on success.
- `prompt-history-search`: feed indexed prompt-history rows to a picker command,
  expose bounded original-prompt previews under
  `$TAU_PROMPT_HISTORY_DIR/<index>`, then replace the prompt with the selected
  original prompt.

Command environment:

- `TAU_EDITOR`: resolved editor command that Tau exposes to prompt shell
  actions, including built-in prompt-edit bindings.
- `TAU_PROMPT_PATH`: tempfile containing the current prompt.
- `TAU_PROMPT_ROW` / `TAU_PROMPT_COLUMN`: 1-indexed cursor position for
  editor commands that support `file:row:column` syntax. Multi-line row
  calculation is still limited.
- `TAU_PROMPT_HISTORY_DIR`: for `prompt-history-search`, a temporary directory
  containing bounded preview files named by row index.

Tau resolves that editor from `$EDITOR`, then `$VISUAL`, then `hx`, `vim`,
`vi`, and `nano` if found on `$PATH`.

`shell-prompt-insert` and `prompt-history-search` capture at most 1 MiB of
stdout and discard stderr. `shell-prompt-edit` inherits terminal stdio so
interactive editors can use the terminal directly. All prompt shell actions time
out after 1 hour. Completion commands from `complete_with_command` capture at
most 256 KiB of stdout, discard stderr, and time out after 10 seconds. History
search uses the newest 200 non-empty prompts, truncates row summaries to 240
characters, and caps preview files to 64 KiB each / 1 MiB total before launching
the picker. Failures are shown as local prompt/completion notices.

Default bindings are documented in
[`docs/cli-keybindings.md`](docs/cli-keybindings.md#built-in-bindings).

A Helix override:

```json5
bind: {
  "C-o": {
    action: "shell-prompt-edit",
    command: 'hx "$TAU_PROMPT_PATH:$TAU_PROMPT_ROW:$TAU_PROMPT_COLUMN"',
  },
  "C-g": {
    action: "shell-prompt-edit",
    command: 'hx "$TAU_PROMPT_PATH:$TAU_PROMPT_ROW:$TAU_PROMPT_COLUMN"',
  },
},
```

Use `trim: true` for commands like `fzf` whose selected value ends with a
newline you do not want inserted into the prompt.

### `Ctrl+O` — edit prompt in your editor

The default `C-o` binding suspends the UI, opens the prompt with Tau's resolved
editor via `$TAU_EDITOR`, and replaces the buffer with whatever you save.
Redraws are paused while the editor owns the terminal.

The editor file also includes a Markdown trailer after:

```md
<!-- TAU trailer: everything after this line will be ignored -->
```

Everything after the marker is ignored when Tau reads the file back. The
trailer quotes useful context for composing the next prompt: the current
in-flight response, the last agent response, and the previous submitted prompt.
If you accidentally edit text below the marker, Tau detects that the trailer
changed and shows the edited text under `Previously edited text below TAU
trailer` the next time you open `$EDITOR`. That recovered text is still not
submitted automatically; move anything you want to keep above the marker. If the
trailer is unchanged, prior recovery is cleared. If you delete the marker line,
the whole file becomes prompt text and prior recovery is cleared. Leading and
trailing blank lines around the editable prompt are trimmed.

### `Ctrl+F` — fzf (or anything else) into the prompt

Because bindings are arbitrary shell commands, wiring fzf or another picker
into the prompt is straightforward:

```json5
bind: {
  "C-f": {
    action: "shell-prompt-insert",
    command: "rg --files --hidden --glob '!.git' | fzf --height=100% --preview 'cat -- {}' --preview-window 'right,60%,wrap'",
    trim: true,
  },
  "C-r": {
    action: "prompt-history-search",
    command: "fzf --height=100% --delimiter='\\t' --with-nth=2 --no-hscroll --preview 'cat \"$TAU_PROMPT_HISTORY_DIR\"/{1}' --preview-window 'right,60%,wrap' | cut -f1",
    trim: true,
  },
  "C-t": {
    action: "shell-prompt-insert",
    command: "RG_PREFIX='rg --line-number --column --no-heading --color=always --smart-case'; fzf --height=100% --ansi --disabled --bind \"change:reload:$RG_PREFIX {q} || true\" --delimiter : --preview 'bat --color=always --style=numbers --highlight-line {2} -- {1} 2>/dev/null || awk -v line={2} '\\''line - 4 <= NR && NR <= line + 4 { printf \"%6d  %s\\n\", NR, $0 }'\\'' -- {1}' --preview-window '+{2}/2' | cut -d: -f1",
    trim: true,
  },
  "C-y": {
    action: "shell-prompt-insert",
    command: "if command -v jj >/dev/null 2>&1 && jj root --ignore-working-copy >/dev/null 2>&1; then jj log -r '::@' --no-graph -T 'change_id.shortest(8) ++ \"\\t\" ++ description.first_line() ++ \"\\n\"' | awk 'BEGIN { OFS=\"\\t\" } { id=$0; sub(/\\t.*/, \"\", id); title=$0; sub(/^[^\\t]*\\t?/, \"\", title); if (title == \"\") title=\"(no description set)\"; if (length(title) < 81) short=title; else short=substr(title, 1, 77) \"...\"; print id, short }' | fzf --height=100% --delimiter='\\t' --with-nth=2 --preview 'jj show --color=always {1}' --preview-window 'right,50%,wrap' | cut -f1; elif command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then git log --format='%h%x09%s' | awk 'BEGIN { OFS=\"\\t\" } { id=$0; sub(/\\t.*/, \"\", id); title=$0; sub(/^[^\\t]*\\t?/, \"\", title); if (title == \"\") title=\"(no description set)\"; if (length(title) < 81) short=title; else short=substr(title, 1, 77) \"...\"; print id, short }' | fzf --height=100% --delimiter='\\t' --with-nth=2 --preview 'git show --color=always {1}' --preview-window 'right,50%,wrap' | cut -f1; fi",
    trim: true,
  },
},
```

`C-f` lists files, shows the selected file in an fzf preview pane, and inserts
the selected path at the cursor. `C-r` searches prompt history (newest first),
shows the selected prompt in an fzf preview pane, and replaces the current draft
with the selected original prompt; `C-z` restores the draft that was active
before the picker opened. `C-t` starts with an empty result list; type a query
to search file contents with `rg`, preview the matching context, and insert the
selected file path. `C-y` opens a jj change picker when inside a jj repository,
falls back to git
commits in git repositories, and inserts the selected change or commit id.
Replace `rg --files | fzf`, the content-search command, or the commit picker
with `git ls-files`, a custom script, or whatever fits your workflow.

### Thinking / reasoning rendering

When the model emits reasoning blocks, the UI renders them inline above the
final answer, styled distinctly from the response. `/set show-thinking <true|false>`
toggles visibility globally; past blocks re-render in place when the flag flips, so
you can hide them after the fact. Reasoning blocks are not replayed back to
the provider as input — they remain provider-side context.

### Diff rendering

File mutations made by `edit` render as inline diffs. By default they collapse
to a compact `+N/-M` chip; `/set show-diff true` expands them to the full
unified hunk view. The terminal renderer uses cell-level differential updates to
avoid full repaints on each token.

### Theming

The UI ships with a conservative built-in `tau-plain-dark` theme that keeps text
attributes and limits hard-coded foreground colors to default color plus yellow,
cyan, green, and red so it remains readable on unusual terminal palettes.
More opinionated built-ins include `tau-dpc` (the previous Tau theme) and
`tau-plain-light`. `cli.yaml` can set `theme: tau-plain-dark`,
`theme: tau-plain-light`, `theme: tau-dpc`, or a custom theme name; `TAU_THEME`
accepts the same names and overrides config for one process. Custom themes load
from `~/.config/tau/themes/<name>.json5` (or the active Tau config directory)
and Tau fails visibly if a configured theme is missing or malformed. Themes can
include an optional `description` shown in `/theme` completion and the
no-argument `/theme` available-theme listing, and map semantic style names
(`prompt.marker`, `prompt.cwd`, `banner.accent`, `system.info`, diff hunks,
reasoning blocks, …) to terminal attributes. Style attributes include `fg`,
`bg`, `bold`, `underline`, `italic`, and
`strikethrough`; `strikethrough` maps to terminal crossed-out SGR where the
terminal supports it. See `crates/tau-themes/themes/tau-plain-dark.json5`,
`tau-plain-light.json5`, and `tau-dpc.json5` for built-in examples.

### Session resume and detach

`/detach` leaves the harness daemon running so the agent can keep working in
the background; `tau --attach` reconnects later. `tau -r` opens a picker for
recent sessions in the current `cwd` (showing lock status and the latest user
prompt), `tau -r <id>` picks a specific one. Session membership and loaded agent
trees, including abandoned branches, are preserved across restarts.

Use `tau --ephemeral` for a non-resumable live session that does not leave
session directories or session logs behind. Agent histories are still durable,
so use a separate agent mode (not this flag) if you need non-persistent agents.

Use `/new` followed by `/ephemeral on` (or bare `/ephemeral` to toggle) before
the first prompt to create an
ephemeral agent. Ephemeral agents keep normal live behavior, can be reattached to
while the same daemon is running, and compose with staged `/model`, but their
agent transcript and session membership are memory-only and disappear on daemon
exit/cold resume. Children spawned by an ephemeral parent default to ephemeral;
session `--ephemeral` remains independent and does not make agents ephemeral by
default.


## XMPP agent chat bridge

Tau includes a disabled-by-default `std-xmpp` extension for personal chat with
agents over XMPP. Agents opt in with `xmpp_register`, reply with `xmpp_send`,
and cannot choose arbitrary destination JIDs. The recommended configuration uses
a MUC room per Tau session id and agent id, sends an XEP-0045 mediated invite
plus a direct fallback notice, and lets multiple Tau processes share one XMPP
account without resource conflicts. Tau joins the room, leaves it on unregister
or session shutdown, and enforces allowlisted real sender JIDs, but the MVP
relies on server configuration/defaults for private, hidden, and members-only
room policy. The MVP sends ordinary plaintext XMPP messages over TLS only; it
does not provide OMEMO or other end-to-end encryption.
