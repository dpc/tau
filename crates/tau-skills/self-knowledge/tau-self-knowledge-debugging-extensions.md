---
name: tau-self-knowledge-debugging-extensions
description: Use when debugging a supervised Tau extension’s startup, crash, reconnection, stderr logs, tracing filter, or diagnostic privacy boundary.
advertise: false
---

# Debugging Tau extensions

## Enable tracing

Set `TAU_LOG` when starting/restarting the harness:

```sh
TAU_LOG='extension_target=debug,warn' tau
```

Use the extension's documented target; `trace` helps with timing/reconnects.
The value uses `RUST_LOG`/`EnvFilter` syntax and replaces the normal filter;
include `warn` to retain unrelated warnings. Children inherit the harness
environment except secrets and read the filter at startup. Setting `TAU_LOG`
on a later `tau attach` changes only that new UI.

## Inspect one session

Find the session with `tau session list`, then read extension and harness logs
together. Filenames use configured instance names, not necessarily `std-slack`.

```sh
logs="${XDG_STATE_HOME:-$HOME/.local/state}/tau/sessions/<session_id>/logs"
find "$logs" -maxdepth 1 -type f -name '*.log' -printf '%f\n'
tail -n 200 "$logs/tau-harness.log"
tail -n 200 "$logs/<extension-instance>.log"
```

Repeated stderr-close markers indicate process exits. A live process may instead
be reconnecting internally; use its target/runbook to interpret connection,
handshake, heartbeat, and retry records. Restart after fixing environment,
configuration, credentials, or reachability; attaching does not update a child.

## Privacy and mirroring

`logs/<extension-instance>.log` contains authoritative raw stderr: unredacted
at the sink, unrotated within a session, and subject to whole-session retention
(disabled by default), not shorter diagnostic cleanup. Review before sharing;
even first-party redaction does not make the whole file public-safe.

`tau serve --mirror-extension-stderr` adds a default-off, framed, escaped,
bounded, lossy copy to process stderr. It exposes data to a wider journal-reader
audience and cannot replace the private raw file for complete bytes or ordering.
