# CLI keybindings

Keep this document in sync with
`crates/tau-config/config/built-in.cli-bindings.yaml`, prompt-local action
handling in `crates/tau-cli-term/src/lib.rs` and
`crates/tau-cli-term-raw/src/lib.rs`, application action handling in
`crates/tau-cli/src/chat.rs`, built-in completion triggers in
`crates/tau-cli-term/src/completion.rs` (`CompletionRules::built_in()`), and the
sample `config/cli.yaml`.


## Built-in bindings

| Key | Action | Description |
| --- | --- | --- |
| `Enter`, `C-Enter` | `submit-prompt` | Submit the prompt, or accept a previewed completion without submitting. |
| `C-c` | `clear-or-cancel-prompt` | Clear a non-empty prompt, or request prompt cancellation on a second press when empty. |
| `C-d` | `prompt-eof` | Signal EOF when the prompt is empty. |
| `C-a`, `Home` | `cursor-start` | Move to the beginning of the prompt. |
| `C-e`, `End` | `cursor-end` | Move to the end of the prompt. |
| `C-u` | `kill-to-start` | Kill from cursor to the beginning of the prompt. |
| `C-w` | `kill-word-left` | Kill the word before the cursor. |
| `Backspace` | `delete-backward` | Delete the character before the cursor. |
| `Delete` | `delete-forward` | Delete the character after the cursor. |
| `Left`, `Right` | `cursor-left`, `cursor-right` | Move by one character. |
| `Up`, `Down` | `cursor-up`, `cursor-down` | Cycle completion candidates, move/scroll within capped multiline input, then step prompt history at the input edges. |
| `Esc` | `escape` | Dismiss the completion menu if open, otherwise surface Escape. |
| `C-b` | `agent-pick` | Pick a currently active agent with optional `fzf`. |
| `C-f` | `shell-prompt-insert` | Pick a file with `fzf`, preview the highlighted file, and insert it at the cursor. |
| `C-k` | `agent-previous` | Cycle to the previous active agent or overview. |
| `C-j` | `agent-next` | Cycle to the next active agent or overview. |
| `C-r` | `prompt-history-search` | Search past prompts with `fzf`, preview the highlighted prompt, and replace the current prompt with the selected prompt. |
| `C-t` | `shell-prompt-insert` | Search files with ripgrep through `fzf` and insert the selected path. |
| `Tab` | `cycle-role` | Cycle roles within the current role group. |
| `BackTab` / `Shift-Tab` | `cycle-role-group` | Cycle to the first role in the next role group. |
| `C-p`, `C-Up` | `prompt-previous` | Move to the previous prompt/history entry. |
| `C-n`, `C-Down` | `prompt-next` | Move to the next prompt/history entry. |
| `C-z` | `prompt-undo` | Undo the last edit in the current prompt/history entry. |
| `C-y` | `shell-prompt-insert` | Pick a jj change or git commit with `fzf` and insert its id at the cursor. |
| `C-o`, `C-g` | `shell-prompt-edit` | Edit the current prompt with `$TAU_EDITOR`, falling back through `$EDITOR`, `$VISUAL`, `hx`, `vim`, `vi`, then `nano`. |
## Built-in file completion triggers

Typing any of the following prefixes at the prompt triggers inline path completion:

| Prefix | Behavior |
| --- | --- |
| `./` | Directory prefix matching in the current directory. Configure `complete_path_fuzzy` to prefer fuzzy git-tracked/unignored file matches for this prefix. |
| `../` | Directory prefix matching in the parent directory. |
| `:` | Intrinsic command/action completion when it is the first non-whitespace character; `::text` submits literal `:text`. |
| `/` | Filesystem path completion for leading absolute paths and path tokens later in a prompt. |
| `~`, `~/` | Directory prefix matching in the home directory. |

`@...` is intentionally not a file completion trigger; it remains reserved for
agent mention completion.


## Built-in editing keys

These keys are handled by named actions in the default binding file, with raw fallback behavior when no configurable binding matches. The built-in `Enter` binding makes plain Enter submit by default; bind `Enter` to `insert-newline` to restore the raw editing fallback.

| Key | Behavior |
| --- | --- |
| `Enter` | Insert a newline when not bound; submits by default via the built-in binding. |
| `C-Enter` | Submit the prompt. |
| `Shift-Enter`, `Alt-Enter` | Insert a newline. |
| `C-d` on an empty prompt | Exit Tau when no agent/session work is in progress; otherwise print a notice to use `:quit` and keep the session running. |
| `C-c` on an empty prompt | Arm cancellation and print `Press Ctrl-C again to cancel the current response; use Ctrl-D to exit`; a second consecutive `C-c` cancels. |
| `C-c` on a non-empty prompt | Clear the prompt; undoable with `prompt-undo`. |
| `C-a` / `Home` | Move to the beginning of the prompt. |
| `C-e` / `End` | Move to the end of the prompt. |
| `C-u` | Kill from cursor to the beginning of the prompt. |
| `C-w` | Kill the word before the cursor. |
| `Backspace`, `Delete` | Delete text around the cursor. |
| Arrow keys | Cycle completion candidates, move/scroll within capped multiline input, then step prompt history at the input edges. |
| `Shift-Tab` | Cycle completion candidates backward when a completion menu is open; this takes precedence over configured `BackTab` bindings. Otherwise this is configurable as `BackTab`. |
| `Esc` | Dismiss the completion menu. |


## Configurable actions

Bindings live under `cli.bind` in config. The built-in bindings are merged below user bindings, so configuring one key does not remove the rest.
Control-letter bindings are case-sensitive: for example, `C-b` means Ctrl+B
without Shift, while `C-B` means Ctrl+Shift+B. Canonical
`M-<ascii-character>` denotes an exact Alt-only character event; the uppercase
`M-` prefix and exactly one ASCII suffix character are required. For example,
`M-a` means Alt+A and works with both Crossterm Alt events and the legacy
terminal `ESC a` encoding.

Tau does not reserve a Meta chord by default. Explicit user bindings may use
Meta chords or `C-B` on terminal paths that preserve Shift.

- `submit-prompt` — submit the current prompt, or accept a previewed completion without submitting.
- `insert-newline` — insert a newline at the cursor.
- `prompt-eof` — signal EOF when the prompt is empty.
- `clear-prompt` — clear a non-empty prompt.
- `clear-or-cancel-prompt` — clear a non-empty prompt, or arm/trigger cancellation on an empty prompt.
- `cursor-start` / `cursor-end` — move to the beginning or end of the prompt.
- `cursor-left` / `cursor-right` — move one character left or right.
- `cursor-up` / `cursor-down` — cycle completion candidates, move vertically/scroll locally in capped multiline input, or step prompt history after reaching the input edge.
- `move-up` / `move-down` — move vertically inside multiline input only.
- `delete-backward` / `delete-forward` — delete around the cursor.
- `kill-to-start` — kill from cursor to the beginning of the prompt.
- `kill-word-left` — kill the word before the cursor.
- `select-completion-next` / `select-completion-previous` — cycle completion candidates when the menu is open.
- `accept-completion` — accept the previewed completion candidate when available.
- `dismiss-completion` — dismiss the completion menu when open.
- `escape` / `backtab` — surface Escape or BackTab to the outer UI.
- `prompt-previous` — move backward in prompt history, bypassing prompt-local scrolling.
- `prompt-next` — move forward in prompt history, bypassing prompt-local scrolling.
- `prompt-undo` — undo an edit in the current prompt/history entry.
- `prompt-redo` — redo an undone edit in the current prompt/history entry.
- `fast-toggle` — toggle fast mode without editing the prompt draft.
- `cycle-role` — cycle roles within the current role group.
- `cycle-role-group` — cycle to the first role in the next role group.
- `agent-previous` — cycle to the previous active agent or all-agent overview.
- `agent-next` — cycle to the next active agent or all-agent overview.
- `agent-pick` / `agent-pick-all` — invoke `fzf` directly over a bounded
  current-session agent snapshot. The active picker admits unconditional
  `active` agents and running `active_auto` agents; the all picker also admits
   idle `active_auto` and explicitly suspended agents. Both project current
   work phase and running state as compact emoji, retain the task title, omit
   lifecycle and role, and revalidate the selected id. See the linked picker
   documentation for the exact legend.
  Cancel, failure, and stale selections preserve the current transcript and
  prompt draft. Selection alone preserves navigation mode; a later accepted
  visible prompt to that selected existing agent makes it `active`. See
   [Listing and picking agents](list-agents.md).
  `agent-pick` is bound to `C-b` by default; `agent-pick-all` is unbound by
  default. The same actions are available without a binding as
  `:pick-agent` and `:pick-agent-all`.
- `prompt-history-search` — feed indexed prompt-history rows
  (`<index>\t<single-line summary>`) to `command`; bounded original-prompt
  previews are also written under `$TAU_PROMPT_HISTORY_DIR/<index>`. Replace the
  prompt with the selected row's original prompt. The current draft is recorded
  for `prompt-undo` before the picker opens. History search uses the newest 200
  non-empty prompts, truncates row summaries to 240 characters, and caps preview
  files to 64 KiB each / 1 MiB total before launching the picker.
- `shell-prompt-insert` — run `command` and insert stdout at the cursor.
- `shell-prompt-edit` — run `command` with the current prompt in
  `$TAU_PROMPT_PATH` and replace the prompt with the edited file content. When
  Tau adds its `TAU trailer` marker, text below the marker is ignored unless it
  changed during editing: changed trailer text is shown under `Previously edited
  text below TAU trailer` on the next editor open so you can manually move it
  above the marker. Leaving the trailer unchanged clears old recovery. Deleting
  the marker makes the whole file prompt-owned and also clears old recovery.

## Prompt-history persistence

Tau updates the current process's prompt navigation immediately. Persistence
uses a bounded best-effort FIFO worker: admission queues a copy, not a durable
write. If its item or byte limit is full, Tau drops the newest history entry
instead of delaying prompt submission. Tau neither flushes nor fsyncs these
writes, and it does not drain the worker on exit. See
[SECURITY.md](../SECURITY.md#prompt-history-persistence) for the complete
cooperative multi-process file contract.

`shell-prompt-insert` and `prompt-history-search` capture at most 1 MiB of
stdout and discard stderr. `shell-prompt-edit` inherits terminal stdio so
interactive editors can use the terminal directly. All prompt shell actions time
out after 1 hour. `complete_with_command` completion commands capture at most
256 KiB of stdout, discard stderr, and time out after 10 seconds. Failures are
shown as local prompt/completion notices rather than submitted to the agent.
