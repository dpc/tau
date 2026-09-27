---
name: tau-self-knowledge-ext-utils
description: Use for Tau std-utils timer wakeups, daily reminders, papercut reporting, private report history, and operator papercut inspection.
advertise: false
---

# Standard utility tools

`std-utils` provides `timer` and the default-enabled best-effort `papercut`
reporter. It also supplies artifact-backed `read_image`; load
`tau-self-knowledge-ext-shell` for shell artifact transfer and
`docs/artifacts.md` for image/reference details.

Use `timer` for real-time external waits, not to poll for tool completion
(use `wait` for that). Timers belong to the session; they can wake a running
agent, but they do not keep a session open. The tool supports one-shot relative
delay, optional repeating interval, and daily wall-clock scheduling:

```json
{"action":"schedule","timer_id":"agenda","daily_time":"08:00","message":"prepare today's agenda"}
```

Daily time follows the host's local timezone unless `"utc": true`; use
`{"action":"list"}` and `{"action":"cancel","timer_id":"agenda"}` to inspect
or cancel. Accepted daily timers reconstruct from session replay, but a
stopped session cannot wake itself; timezone changes and downtime affect
firing. See `crates/tau-ext-utils/README.md` for precise DST/restart behavior.

Call `papercut` only for an incidental Tau harness, tooling, environment,
confusing, or suspicious problem, then continue the primary task. Do not
report a non-event or retry a failed report. It records local plaintext notes
in per-instance User-scope extension state; do not include secrets or
unnecessary personal data. Reports can be lost in memory-only mode or on
storage/shutdown failure. Active reports and archived files are retained
indefinitely, not automatically filed, redacted, rotated, or deleted.

Operators can review the normal instance with `tau dev papercut list
--markdown`; `tau dev papercut clear` **archives**, rather than deletes,
the active records and prints the archive path. Agent history access is
separately opt-in: set
`extensions.std-utils.config.papercut_history.enable: true` **and** grant
`papercut_list`, `papercut_read`, and/or `papercut_archive` explicitly to
the intended role. Enabling ordinary `papercut` does not grant history.
List shows metadata, read returns full reports, and archive preserves the
whole active file. Keep this grant narrow; the tools inspect shared
per-instance records, not just the calling agent's reports. See
`crates/tau-ext-utils/README.md` for config examples and storage limits.
