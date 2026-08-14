# Listing and picking session agents

`tau agent list <session-id>` queries the live harness currently bound to that
session. It does not start a daemon and does not inspect unrelated agent
directories.

By default it prints live agents whose harness-owned navigation mode is not
`suspended`. Idle `active_auto` agents are included: this command's visibility
rule is intentionally broader than C-j/C-k automatic navigation.

Options are additive:

- `--include-suspended` includes suspended live agents.
- `--include-unavailable` includes current members without a live runtime and
  rows whose creation facts are missing, invalid, or unreadable.
- `--include-unloaded` includes agents previously loaded in the session.
- `--all` enables all three options.

## Output

Output is headerless TSV, one agent per line, in stable parent-before-child
order. Ready rows are ordered by known creation timestamp and then agent id.
Malformed parent cycles are broken deterministically without dropping rows.

The eleven fields are:

1. stable agent id
2. lifecycle: `live`, `unavailable`, or `unloaded`
3. runtime: `idle`, `running`, or `-`
4. detailed activity: `responding`, `manipulating`, `fetching`, `waiting`,
   `timer_scheduled`, `idle`, or `-`
5. navigation: `active`, `active_auto`, `suspended`, or `-`
6. persistence: `durable` or `ephemeral`
7. creation facts: `available`, `missing`, `invalid`, or `unreadable`
8. creation role or `-`
9. parent agent id or `-`
10. creation timestamp in Unix microseconds or `-`
11. current in-memory or stored checkpoint display name, or `-`

Free-text fields escape backslash, tab, newline, carriage return, and other
controls. The first field is always the unescaped agent id, so scripts can use:

```sh
tau agent list my-session | cut -f1
tau agent list my-session --all | fzf --delimiter=$'\t'
```

An empty result is successful and prints nothing. A missing/ambiguous daemon,
stale session, or fixed snapshot bound failure is an error and prints no partial
rows. One-shot requests have a ten-second absolute deadline. A closed downstream
pipe is treated as normal command completion.

## Attached picker

In the terminal UI, C-b and `:pick-agent` open `fzf` over currently active
rows: unconditional `active` agents plus `active_auto` agents whose runtime is
running. `:pick-agent-all` opens it over all current live rows, including idle
`active_auto` and explicitly suspended agents. The underlying `agent-pick` and
`agent-pick-all` actions remain configurable; the all-agent action has no
default key binding. This eligibility filter remains separate from the runtime
state shown in both pickers.

`fzf` is optional and is started only when a picker command or configured
binding action is used. Tau passes rows through stdin and invokes `fzf` directly
rather than interpolating agent data into a shell command. The picker shows
`<work-status-emoji><turn-emoji> @agent-id`, self/inclusive creator-subtree
estimated API cost, work-status title, and display name in space-padded,
terminal-width-aware columns. The compact legend is `🚀` working, `⛔️` blocked,
`✅` done, and `❓` unreported or unknown. Detailed activity uses `✨` responding, `🔨` manipulating, `🌐` fetching,
`⏳` waiting, `🕔` scheduled timer, and `💤` idle. Lifecycle and role remain available in the
machine-facing roster but are omitted from the picker: picker membership is
already restricted to live agents, and the stable agent id is its primary
identity. Work status comes from the same fresh harness snapshot as roster
membership; an unreported title is `-`. The picker uses the latest
canonical per-agent cost pair processed by the renderer: known zero is
`$.00/$.00`, known values independently use the compact status-line format,
and an unavailable pair is `-/-`. Trailing columns are progressively omitted under width pressure,
keeping identity before status, current-turn state, and cost. Long values are truncated for display only; the
selected stable id and original escaped picker row remain unchanged. This
picker-only cost/status/title/runtime projection does not change the ten-field
`tau agent list` output.

Canceling the picker, a missing `fzf`, malformed output, or a stale selection
does not change the selected transcript or prompt draft. Before switching, Tau
rechecks the session, live lifecycle, and the selected picker's eligibility
rule. Selecting through the all-agent picker does not resume a suspended agent.
The picker never loads or resumes an unloaded agent. A later successfully
admitted visible prompt to that selected existing agent does make it `active`.
After settling `fzf`, Tau checks that it regained foreground terminal ownership
before resuming raw input or redraw. The fatal restoration error preserves the
picker outcome and restoration error, keeps terminal output paused, and exits
only that attachment while leaving the harness available for another attach.

Each attached terminal owns its selected transcript, editable prompt draft,
runtime theme, and redraw state. These visual and editing differences do not
alter another attached UI or submit provider work. Draft edits can still publish
the separately specified live prompt-draft liveness observation.
Terminal dimensions are local too: resizing one attached UI may change wrapping,
spacing, adaptive field elision, and truncation positions, but not another UI's
selected agent or the shared semantic transcript.
