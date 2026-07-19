---
name: tau-self-knowledge-cli-ui
description: >
  Use this skill when the user asks about Tau's terminal CLI UI, prompt input,
  slash commands, prompt history, key bindings, or prompt completions.
advertise: false
---

# Tau CLI UI

Tau's terminal UI is the interactive `tau` client. It connects to a harness daemon, renders the transcript, and owns prompt input behavior such as slash commands, history, key bindings, external editor integration, and prompt completions.

## Configuration files

CLI UI configuration lives under `~/.config/tau/`:

- `cli.yaml` — main CLI display, key binding, and completion settings.
- `cli.d/*.yaml` — drop-in CLI overrides layered after `cli.yaml`.

Runtime UI toggles changed with `/set` are stored in the state directory as `cli.json`.

## Agent message labels

Harness-owned `message` activity keeps each agent's routing id visible and adds
the known display/task name in parentheses, independently for sender and
recipient. Unknown agents and peers without trustworthy advertised metadata
remain id-only; the human endpoint remains `user`. Display names are escaped,
bounded presentation metadata and never alter semantic message content or
routing identity. Late authoritative name updates reproject historical UI
blocks without rewriting the stored message event.

## Watched-agent activity

The terminal shows one activity row per direct watched target. `running` means
that directed edge's outer-turn lifecycle is directly running. `watching` means
the directed edge is not running but its target recursively watches an active
descendant; the row's `-> @agent-id` suffix identifies a nearest directly running
witness. Direct state wins when both apply. Recursive activity is a CLI-only
projection over the session's live watch DAG and does not change navigation,
routing, persistence, or model-visible watch notifications. The bottom `@N` chip
keeps its session-wide scope, counts unique recursively effective watch targets,
excludes the selected agent, and retains active-prompt fallback for agents
outside every watch edge.

## All-agent overview

The no-agent-selected screen is both the start-new-agent input target and an
overview of messages between all agents. It deduplicates sender and recipient
projections of the same message and follows `/set show-messages`. Ctrl-K/Ctrl-J
include the overview when cycling among active agents. Submitting a prompt there
still starts a new agent. The overview is local to that CLI: attachment catch-up
includes currently loaded agents, not messages whose endpoints already unloaded.

## Slash commands

Type `/` as the first non-whitespace character in the prompt to open slash/action completion. Built-in commands include session and agent management, model/role switching, `/name <display name>` to rename the currently selected agent, `/skill <name> [args]` for explicit user-invocable skill injection, `/theme <name>` to switch only the current CLI UI's theme for this run, `/set`, `/tree`, `/fast`, `/detach`, and `/quit`. Extension-provided actions can add dynamic slash commands and argument completions at runtime. `/skill:<name> [args]` is accepted as a Pi-compatible alias; arguments are appended after the skill body without placeholder substitution.

`/theme` completion lists built-in selectors (`tau-plain-dark`, `tau-plain-light`, and `tau-dpc`) plus valid user themes from `<config_dir>/themes/*.json5`. It is intentionally not persistent: it does not edit `cli.yaml`, update `cli.json`, or affect another attached UI.

`/set notice-level <critical|warning|info|debug|trace>` controls which harness/UI notices this UI shows. The default is `info`; `warning` hides routine lifecycle chatter such as extension ready messages; `debug` and `trace` show progressively noisier developer-oriented notices. Critical and mandatory notices such as extension configuration errors remain visible even with restrictive thresholds.

## Prompt history and editing

Submitted prompts are kept in the current process and persisted under the state directory as `prompt-history.cbor`. Up/Down navigate prompt history. Built-in key bindings also support prompt undo/redo, Ctrl-R history search, Ctrl-O/Ctrl-G external editor integration, and shell-backed prompt insertion commands.

## Prompt completions

Prompt word completions are configured in `cli.yaml` with a `completions` map from trigger prefix to completer spec:

```yaml
completions:
  "@": complete_agents
  "./": complete_path
  "../": complete_path
  "/": complete_path
  "~": complete_path
  "~/": complete_path
  "#/": complete_with_command fzf some arguments
```

The longest matching word prefix wins, except `/` as the first non-whitespace character always opens slash/action completion for now.

Available completers:

- `complete_agents` — complete effectively active agent mentions, preserving
  the trigger prefix. Ordinary agents are always active; delegated
  `active-auto` agents are included only while their complete outer turn is
  running. Explicitly suspended agents are excluded.
- `complete_path` — plain filesystem directory-prefix completion.
- `complete_path_fuzzy` — fuzzy git-tracked path completion for `./<partial>`,
  falling back to directory-prefix completion.
- `complete_actions` — complete slash/action command names; useful for future or
  custom non-leading command triggers.
- `complete_with_command <argv...>` — run the command when the trigger token is
  typed exactly, release the terminal while it runs, trim stdout, and replace the
  trigger token with stdout. These commands run with foreground terminal
  ownership while Tau is paused, capture at most 256 KiB of stdout, discard
  stderr, time out after 10 seconds, and show failures as local completion
  notices. Arguments are currently split on whitespace; use a wrapper
  script for complex shell snippets or argv entries containing spaces.

`shell-prompt-insert` and `prompt-history-search` capture at most 1 MiB of
stdout and discard stderr. `shell-prompt-edit` inherits terminal stdio so
interactive editors can use the terminal directly. All prompt shell actions time
out after 1 hour and show failures as local prompt notices. History search uses
the newest 200 non-empty prompts, truncates row summaries to 240 characters, and
caps preview files to 64 KiB each / 1 MiB total before launching the picker.

The shipped defaults use plain path completion. Configure `./: complete_path_fuzzy`
to opt into fuzzy git path completion for `./<partial>`.
`/retry` runs the selected agent's exact currently delayed provider retry now.
It does not resubmit prompt text; if the provider no longer has that prompt
parked, Tau reports that it may already be running.

## Shared agent navigation modes

Navigation classification is shared by UIs attached to the same daemon. Use
`/agent suspend`, `/agent resume`, or `/agent auto` for absolute `suspended`,
`active`, or `active-auto` modes. Selection, drafts, transcript view, and
presentation remain local to each UI. Overrides survive UI reconnect while the
agent remains loaded in the same daemon session; unload, session switch, and
harness restart forget them.

Use `tau agent list <session-id>` for a stable headerless TSV roster from a
running session; `--include-suspended`, `--include-unavailable`,
`--include-unloaded`, and `--all` widen its default live non-suspended view.
Inside the terminal UI, C-b opens optional `fzf` over the same current
non-suspended category and revalidates the selection before switching.
