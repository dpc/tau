---
name: tau-self-knowledge-cli-ui
description: Use when using or configuring Tau terminal CLI UI, including prompt input, commands, prompt history, key bindings, or prompt completions.
advertise: false
---

# Tau CLI UI

## Configuration files

CLI UI configuration lives under `~/.config/tau/`:

- `cli.yaml` — main CLI display, key binding, and completion settings.
- `cli.d/*.yaml` — drop-in CLI overrides layered after `cli.yaml`.

Runtime UI toggles changed with `:set` are stored in the state directory as `cli.json`.

## Mouse handling

To disable default mouse handling, including wheel-driven prompt history,
set this static option in `cli.yaml` or a drop-in:

```yaml
mouse: false
```

With `mouse: false`, Tau disables terminal mouse reporting so native scrolling,
selection, and links work. This is not a `:set` toggle.

## Agent message labels

Message headers show routing IDs as `@agent` or `session/@agent`, plus known
display/task names in parentheses. Names do not change routing. Later name
updates refresh historical headers, not stored messages; unknown peers stay ID-only.

## Watched-agent activity

Each directly watched target gets a row: `running` for its own activity,
`watching` if idle but recursively watching an active descendant.
`-> @agent-id` names a nearest running witness; direct activity wins.
This recursive display does not change model-visible watch notifications.
The session-wide `@N` chip counts unique effective watch targets, excludes
the selected agent, and includes active-prompt fallback outside watch edges.

## Status-indicator legend

Rows start with independent `<work-status><current-activity>` indicators.
`⏳⏳`, for example, means a reported waiting task and a currently waiting tool.

The work-status indicator is:

| Glyph | Reported task status |
| --- | --- |
| `❓` | no status is reported, or a previous status is no longer reliable |
| `🚀` | working |
| `✅` | done |
| `⛔️` | blocked pending external intervention |
| `⏳` | waiting for an expected self-resolving event |

The current-activity indicator is:

| Glyph | Current execution state |
| --- | --- |
| `✨` | the provider is generating a response |
| `🔨` | an active tool is mutating state or has no more-specific category |
| `🌐` | an active tool is fetching data |
| `⏳` | an active tool is waiting |
| `🕔` | no higher-priority work is active and a timer is scheduled |
| `💤` | idle: no response, tool call, or ambient activity is active |

## All-agent overview

The no-agent-selected screen shows deduplicated inter-agent messages, subject
to `:set show-messages`. Ctrl-K/Ctrl-J include it when cycling active agents;
submitting there starts a new agent. Attachment catch-up covers loaded agents,
not messages whose endpoints already unloaded.

## Commands

Type `:` as the first non-whitespace character to enter command mode and open command/action completion. Type `::text` to submit literal prompt text beginning with `:`, while `/` remains available for absolute and token-level path completion. Built-in commands include session and agent management, model/role switching, `:name <display name>` to rename the currently selected agent, `:skill <name> [args]` for explicit user-invocable skill injection, `:theme <name>` to switch only the current CLI UI's theme for this run, `:set`, `:tree`, `:fast`, `:detach`, `:quit`, and `:quit-session`. Extension-provided actions can add dynamic commands and argument completions at runtime. `:skill:<name> [args]` is accepted as a compact form; arguments are appended after the skill body without placeholder substitution.

`:quit` (alias `:q`) leaves this UI. Plain `tau` and interactive `tau resume`
stop when the last UI leaves unless `:detach` has disabled that policy for the
daemon's remaining lifetime. Reconnecting/quitting cannot re-enable it; headless
launches start with it disabled. `:quit-session` shuts down the daemon and all
UIs. Final stderr confirms detach/termination or diagnoses unconfirmed termination.

`:theme` completion lists built-in selectors (`tau-plain-dark`, `tau-plain-light`, and `tau-dpc`) plus valid user themes from `<config_dir>/themes/*.json5`. It is intentionally not persistent: it does not edit `cli.yaml`, update `cli.json`, or affect another attached UI.

`:set notice-level <critical|warning|info|debug|trace>` filters diagnostics
(default `info`). `warning` hides routine lifecycle notices; direct command
responses, alerts, and critical notices remain visible. Compact mode hides
diagnostics and internal prompts even with `show-internal-prompts` on.

## Prompt history and editing

Default `Enter` submits; `Shift-Enter`/`Alt-Enter` inserts a newline.
`Ctrl-C` clears a nonempty draft (undoable). On an empty draft, the first
`Ctrl-C` arms cancellation and a second consecutive press cancels the current
response. `Ctrl-D` exits only when no agent/session work is in progress;
otherwise use `:quit-session` for explicit daemon shutdown. `Tab` cycles roles
within the role group; `BackTab`/`Shift-Tab` cycles groups, unless a completion
menu is open (then it cycles candidates backward). Typing `&` completes a
running session ID, but the inserted ID is still ordinary prompt text.
Some terminals send Backspace as `Ctrl-H`, whose default binding switches
agents; configure Backspace to send DEL or override the binding.

For a paste of at least 8 KiB, Tau uploads normalized text and inserts an
editable artifact reference rather than submitting the text. See
`tau-self-knowledge-artifacts` for upload failure, Enter retry, Ctrl-C
discard, and retention behavior. `docs/cli-keybindings.md` is the complete
key reference.

Submitted prompts are kept in the current process and persisted under the state directory as `prompt-history.cbor`. Up/Down navigate prompt history. Built-in key bindings also support prompt undo/redo, Ctrl-R history search, Ctrl-O/Ctrl-G external editor integration, and shell-backed prompt insertion commands. `:edit-prompt [response_rel_idx]` opens the editor with a selected prior response, where zero is newest. `:edit-prompt-chat` and the shifted `C-O` binding place the complete durable Markdown conversation below the existing trailer marker; terminals that report bare `^O` keep ordinary `C-o`.

## Compact and verbose transcript modes

The CLI starts verbose, honoring `:set` choices. Ctrl-V, the configurable
`verbose-mode-toggle` action, or `:verbose-mode-toggle` switches to compact view.

Compact view hides thinking, turn stats, completed tools/results, and running
tool payloads. Running tool status lines disappear at completion. User/agent
messages, direct responses, alerts, critical notices, input, navigation, and
watched activity remain. `Message` / `WatchResponse` retain headers only;
typed watched-agent status rows disappear; `WatchPrompt` retains content.

Switching back restores the transcript and prior settings. The mode is local
to this attachment, does not persist in `cli.json`, and changes no model context
or harness behavior.

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

The longest matching word prefix wins for prompt-text rules. Intrinsic
first-non-whitespace `:` command mode always wins over configured rules.

Available completers:

- `complete_agents` — complete effectively active agent mentions, preserving
  the trigger prefix. Ordinary agents are always active; delegated
  `active-auto` agents are included only while their complete outer turn is
  running. Explicitly suspended agents are excluded.
- `complete_path` — plain filesystem directory-prefix completion.
- `complete_path_fuzzy` — fuzzy git-tracked path completion for `./<partial>`,
  falling back to directory-prefix completion.
- `complete_actions` — complete action command names; useful for future or
  custom non-leading command triggers.
- `complete_with_command <argv...>` — run the command when the trigger token is
  typed exactly, release the terminal while it runs, trim stdout, and replace the
  trigger token with stdout. These commands run with foreground terminal
  ownership while Tau is paused, capture at most 256 KiB of stdout, discard
  stderr, time out after 10 seconds, and show failures as local completion
  notices. Arguments are currently split on whitespace; use a wrapper
  script for complex shell snippets or argv entries containing spaces.

`shell-prompt-insert` and `prompt-history-search` capture at most 1 MiB of
stdout and discard stderr. `shell-prompt-edit` and `shell-prompt-edit-chat`
inherit terminal stdio so
interactive editors can use the terminal directly. All prompt shell actions time
out after 1 hour and show failures as local prompt notices. History search uses
the newest 200 non-empty prompts, truncates row summaries to 240 characters, and
caps preview files to 64 KiB each / 1 MiB total before launching the picker.

The shipped defaults use plain path completion. Configure `./: complete_path_fuzzy`
to opt into fuzzy git path completion for `./<partial>`.
`:retry` runs the selected agent's exact currently delayed provider retry now.
It does not resubmit prompt text; if the provider no longer has that prompt
parked, Tau reports that it may already be running.

Explicit targets in `:agent switch`, `:agent suspend`, `:agent resume`, `:agent
auto`, and `:agent name` accept either `agent-id` or `@agent-id`. Completion
accepts either spelling and inserts the canonical bare id.

## Shared agent navigation modes

`:agent suspend`, `:agent resume`, and `:agent auto` set shared navigation modes
`suspended`, `active`, and `active-auto`. They survive UI reconnect, not agent
unload or daemon restart. Selection, drafts, and transcript view stay UI-local.
Selection alone does not resume an agent; successful visible prompt submission
to an existing selected agent makes it `active`.

Use `tau agent list <session-id>` for a stable headerless TSV roster from a
running session; `--include-suspended`, `--include-unavailable`,
`--include-unloaded`, and `--all` widen its default live non-suspended view.
Inside the terminal UI, C-b and `:pick-agent` open optional `fzf` over
effectively active agents (`active`, or running `active-auto`), while
`:pick-agent-all` includes every current live agent, including suspended
agents. Both show runtime independently and revalidate the selection before
switching. The underlying `agent-pick` and `agent-pick-all` actions remain
configurable; the all-agent action has no default key binding.
