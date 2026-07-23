# tau-cli-term

`tau-cli-term` is the high-level prompt layer used by Tau's terminal UI. It sits between the application (`tau-cli`) and the raw terminal engine (`tau-cli-term-raw`).

## Responsibilities

This crate owns prompt features that need application-shaped data but should not live in the low-level renderer:

- command and argument completion candidate construction,
- completion menu rendering as `tau-term-screen` styled blocks,
- prompt-local binding actions such as prompt history search, undo/redo, external editing, and shell insertion,
- `$EDITOR` / `$VISUAL` resolution and terminal pause/resume around external programs,
- conversion from raw terminal events into the smaller high-level `Event` API consumed by `tau-cli`.

## Boundaries

`tau-cli-term-raw` owns terminal state and editing mechanics: raw mode, crossterm events, key dispatch, multiline buffer editing, completion menu lifecycle, prompt history navigation, undo/redo, redraw scheduling, and screen rendering.

`tau-cli-term` owns prompt semantics that depend on configured commands or external tools. It may call raw named actions, but it should not duplicate the raw editing state machine.

First-non-whitespace `:` enters intrinsic command mode before configured
prompt-text completion rules run. A doubled prefix is literal prompt syntax:
`::text` remains visible while editing but is stored canonically as `:text` in
both raw editable history and the high-level history-search list.

`tau-cli` owns application behavior. High-level binding actions unknown to this crate are surfaced as `Event::Action(String)` and interpreted by `tau-cli`; this crate should not interpret application actions or provider/role behavior.

Tau-specific prompt content can still be supplied through explicit hooks. For example, `CompletionData::set_agent_mention_completer` lets the application provide `@` mention candidates while this crate owns only token detection, replacement ranges, and menu presentation.

## Binding action model

Bindings arrive from config as strings. The dispatch order is:

1. raw named actions recognized by `tau-cli-term-raw::Term::is_named_action`,
2. prompt-local actions parsed by `PromptShellAction::parse`,
3. simple unknown action names surfaced as `Event::Action(String)` for the application.

Colon-form prompt actions are crate-local protocols:

- `shell-prompt-insert:<mode>:<command>` inserts command stdout at the cursor,
- `shell-prompt-edit:<mode>:<command>` edits the current prompt through a temp file,
- `prompt-history-search:<mode>:<command>` feeds indexed prompt history rows to a picker command.

The `<mode>` segment is `trim` to strip leading/trailing command output whitespace
or `raw` to preserve command output as-is.

Malformed or unknown colon-form actions fail locally instead of being forwarded as application actions.

## History model

The raw layer is authoritative for editable prompt-history navigation. This crate keeps a submitted-prompt list only for prompt-history search input. When a prompt is submitted, it is appended here and the raw layer has already recorded the editable history entry.

## External editor context

`EditorContext` is shared with the application, but it is intentionally just text context for prompt editing templates. Keep application-specific interpretation outside this crate.
