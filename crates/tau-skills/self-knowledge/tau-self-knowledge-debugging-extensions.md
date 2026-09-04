---
name: tau-self-knowledge-debugging-extensions
description: >
  Use this skill to debug a supervised Tau extension's startup, crash,
  reconnection, stderr logs, tracing filter, or diagnostic privacy boundary.
advertise: false
---

# Debugging Tau extensions

## Enable the right tracing

Set `TAU_LOG` in the environment that starts or restarts the harness, for
example:

```sh
TAU_LOG='extension_target=debug,warn' tau
```

Use the extension's documented tracing target and level; `trace` is useful for
timing or reconnect investigation. `TAU_LOG` uses `RUST_LOG`/`EnvFilter`
directive syntax. A valid explicit value replaces the component's normal
filter, so include `warn` when warnings from unrelated targets should remain
visible. Supervised extensions inherit the harness environment, except secret
variables. They read the filter when they start. Setting `TAU_LOG` on a later
`tau attach` changes only that new UI process, not existing harness or
extension children.

## Inspect one session

1. Identify the session with `tau session list`.
2. Inspect its private log directory. Extension files use the configured
   instance name, so do not assume a filename such as `std-slack.log`.
3. Read the extension's recent stderr and `tau-harness.log` together. An
   extension log that repeatedly ends at a stderr-close marker indicates
   process exits; a live process can instead have an internal reconnect loop,
   which its extension-specific target should describe.

```sh
logs="${XDG_STATE_HOME:-$HOME/.local/state}/tau/sessions/<session_id>/logs"
find "$logs" -maxdepth 1 -type f -name '*.log' -printf '%f\n'
tail -n 200 "$logs/tau-harness.log"
tail -n 200 "$logs/<extension-instance>.log"
```

For a crash, use the last extension lines plus the matching harness lines to
find startup/configuration or supervision failures. For a reconnect loop, keep
the same bounded window and use the extension's runbook to interpret its
connection, handshake, heartbeat, and retry categories. Restart after correcting
environment, configuration, credentials, or network reachability; do not infer
that an attach changed an existing child.

## Treat stderr as private

`logs/<extension-instance>.log` is the authoritative raw stderr file. `tau
serve --mirror-extension-stderr` is optional and default-off; its process-stderr
copy is framed, escaped, bounded, lossy, and visible to a wider journal-reader
audience. It cannot replace the private raw file for complete bytes or ordering.

Raw extension stderr is unredacted at the sink boundary. It can contain
identifiers, queries, filesystem paths, or custom-extension text even when a
first-party extension intentionally redacts sensitive values from its own
records. Do not share it without review. It is unrotated within the session and
follows whole-session retention (60 days by default), rather than the shorter
diagnostic cleanup period.
