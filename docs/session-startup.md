# Session startup

Plain `tau` creates a new session and owns its new harness daemon. Targeted
startup uses two explicit commands:

```text
tau attach [SESSION]
tau resume [SESSION]
```

`attach` connects to the live daemon that currently advertises `SESSION` and
does not take ownership of it. `resume` verifies a persisted session and starts
a new daemon for it. An unavailable attach target suggests `tau resume
SESSION`; a missing resume target reports that no persisted session exists.
Invalid session IDs fail before daemon startup.

Startup options remain root options and precede the target command, for example
`tau --prompt-stdin resume SESSION`. They are not repeated after `attach` or
`resume`.

When `SESSION` is omitted, attach opens a picker. Attach choices come from
responsive daemon identities and show both session ID and project root. Resume
choices come from persisted metadata, newest first. Tau auto-selects exactly one
unlocked persisted target; with several unlocked targets it opens a picker and
disables locked rows; if every target is locked, it suggests attaching instead. A
non-interactive invocation cannot use a picker and must pass an explicit session
ID.

Previously, `--attach` selected the first responsive daemon for the current
directory, while bare `--resume` selected the newest persisted session
automatically outside a terminal and could create a fresh session when nothing
was selected. The explicit commands now make the action visible, resolve
explicit IDs deterministically, and fail instead of changing the requested
action.
