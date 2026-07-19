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

The ten fields are:

1. stable agent id
2. lifecycle: `live`, `unavailable`, or `unloaded`
3. runtime: `idle`, `running`, or `-`
4. navigation: `active`, `active_auto`, `suspended`, or `-`
5. persistence: `durable` or `ephemeral`
6. creation facts: `available`, `missing`, `invalid`, or `unreadable`
7. creation role or `-`
8. parent agent id or `-`
9. creation timestamp in Unix microseconds or `-`
10. current in-memory or stored checkpoint display name, or `-`

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

In the terminal UI, C-b opens `fzf` over the current live, non-suspended rows.
`fzf` is optional and is started only when the binding is used. Tau passes rows
through stdin and invokes `fzf` directly rather than interpolating agent data
into a shell command. The picker shows agent id, role, display name, lifecycle,
and runtime in space-padded, terminal-width-aware columns. Long values are
truncated for display only; the selected stable id and original escaped TSV row
remain unchanged.

Canceling the picker, a missing `fzf`, malformed output, or a stale selection
does not change the selected transcript or prompt draft. Before switching, Tau
rechecks the session, live lifecycle, and non-suspended mode. The picker never
loads or resumes an unloaded agent.
