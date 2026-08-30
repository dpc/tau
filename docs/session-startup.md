# Session startup

Plain `tau` creates a new session and owns its new harness daemon. Targeted
startup uses three explicit commands:

```text
tau attach [SESSION]
tau resume [SESSION]
tau serve --session SESSION --create
tau serve --session SESSION --existing
```

`attach` connects to the live daemon that currently advertises `SESSION` and
does not take ownership of it. `resume` verifies a persisted session and starts
a new daemon for it. An unavailable attach target suggests `tau resume
SESSION`; a missing resume target reports that no persisted session exists.
Invalid session IDs fail before daemon startup.

`serve` is the foreground supervisor entrypoint for one fixed persisted session.
It starts no terminal UI, remains alive across attachment
disconnects, and publishes the ordinary runtime socket and metadata, so `tau
session list` and `tau attach SESSION` work unchanged. It pins the session and
rejects `:session new`. Exactly one lifecycle guard is mandatory. `--create`
requires the session directory to be completely absent and never resumes,
repairs, or deletes valid or partial state. `--existing` strictly resumes valid
state; missing, locked, or malformed state fails without creation. SIGINT and SIGTERM
stop listener admission, shut down the harness and extensions, remove runtime
socket/metadata, and exit normally. A second SIGINT or SIGTERM forces the
signal's default termination and can interrupt that cleanup.

After a graceful `--create` stop, use the same command with `--existing` for
normal service restarts.

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
