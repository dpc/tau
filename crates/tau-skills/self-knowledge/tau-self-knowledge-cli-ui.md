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

- `complete_agents` — complete active agent mentions, preserving the trigger
  prefix.
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
