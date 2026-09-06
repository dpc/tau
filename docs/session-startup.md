# Session startup

Plain `tau` creates a new session and owns its new harness daemon. Targeted
startup uses three explicit commands:

```text
tau attach [SESSION]
tau resume [SESSION]
tau serve --session SESSION --create
tau serve --session SESSION --existing
tau serve --session SESSION --create-or-existing
```

`attach` connects to the live daemon that currently advertises `SESSION` and
does not take ownership of it. `resume` verifies a persisted session and starts
a new daemon for it. An unavailable attach target suggests `tau resume
SESSION`; a missing resume target reports that no persisted session exists.
Invalid session IDs fail before daemon startup.

Plain `tau` and interactive `tau resume` enable automatic shutdown when the last
UI disconnects. Thus `:quit` (or `:q`) normally terminates the session as a
foreground program would. `:detach` deliberately keeps it running: the harness
clears automatic shutdown before acknowledging the UI's departure. That choice
survives every later reconnection and ordinary quit for this daemon's lifetime.
An attached UI never rearms the policy. With multiple UIs, ordinary quit leaves
the daemon alive until the last UI leaves, unless detach already disabled the
policy. `:quit-session` always requests shutdown, including every attached UI.
After terminal cleanup the UI prints `Session detached` or `Session terminated`
to stderr according to the actual outcome. Failed or unconfirmed termination
gets a diagnostic instead of a success line.

`serve` is the foreground supervisor entrypoint for one fixed persisted session.
It starts no terminal UI, remains alive across attachment
disconnects, and publishes the ordinary runtime socket and metadata, so `tau
session list` and `tau attach SESSION` work unchanged. It pins the session and
rejects `:session new`. Exactly one lifecycle guard is mandatory. `--create`
requires the session directory to be completely absent and never resumes,
repairs, or deletes valid or partial state. `--existing` strictly resumes valid
state; missing, locked, or malformed state fails without creation.
`--create-or-existing` is the idempotent supervisor mode: it atomically claims a
completely absent exact-ID path or strictly resumes valid state. An occupied
partial, malformed, symlinked, or locked path fails unchanged, including torn
journal tails; this mode never deletes, repairs, truncates, replaces, or
overwrites state. SIGINT and SIGTERM
stop listener admission, shut down the harness and extensions, remove runtime
socket/metadata, and exit normally. A second SIGINT or SIGTERM forces the
signal's default termination and can interrupt that cleanup.

Use `--create-or-existing` when one service command must handle both first
provisioning and normal restarts. Keep `--create` and `--existing` for
deployments that require one exact lifecycle assertion.

Supervisors that need extension output in process logs can opt in with
`--mirror-extension-stderr`. The option exists only on `tau serve` and is off by
default:

```text
tau serve --session SESSION --create-or-existing --mirror-extension-stderr
```

Tau keeps each private extension log authoritative and unchanged, then sends
escaped, framed records with extension name, child generation, PID, and
line/chunk/EOF boundary to inherited stderr through a bounded best-effort
worker. Queue saturation can suppress mirror records, and a process-stderr
failure disables mirroring without stopping private-file draining or the
harness. Tau uses an independent duplicate of inherited stderr when setup
succeeds; any setup failure, including descriptor duplication or worker-thread
creation, disables only mirroring. Mirror traffic may still consume capacity in
the shared stderr sink, and ordinary harness tracing remains synchronous. Do not
join or pipe-follow extension log files to reproduce this
behavior. For a user systemd service, route `StandardError=journal`; journald
owns journal retention and may apply its own suppression.
The exact record grammar, UTF-8-aware 4096-byte framing, canonical escaping,
drop notices, and within-generation ordering contract are documented under
[Extension logs](extensions.md#extension-logs).

`serve` can admit one literal prompt after complete readiness:

```text
tau serve --session SESSION --create-or-existing \
  --bootstrap-prompt-file /run/credentials/tau-bootstrap \
  --bootstrap-id telegram-v1
```

Both bootstrap options are required together. The id contains 1–128 ASCII
letters, digits, underscores, or hyphens. `PATH=-` reads stdin through EOF once;
stdin EOF never stops the service. A missing, unreadable, empty, non-UTF-8 source
fails before durable bootstrap creation; whitespace is content.

The harness creates one parentless durable user agent with the selected/default
role and admits the bytes as a literal prompt through the normal local UI path.
It waits only for `Created` plus `Queued`, not model output, then stays alive.
The new agent's sequence-zero creation fact stores a reserved non-inheritable
marker. A restart with the same id skips without reading the source; a different
id starts a new generation. This is deliberately at-most-once: once the marker
commits, any ambiguous later failure remains skipped. Attach to inspect/recover,
or choose a new id only to request another attempt. Keep prompt files out of the
Nix store and use service-manager credentials for secrets.

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
