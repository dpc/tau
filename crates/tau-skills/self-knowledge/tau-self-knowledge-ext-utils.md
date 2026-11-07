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

`read_image` prepares encoded pixels locally. Persistent invocations with scoped
storage authority use durable Artifact references; ephemeral/memory-only calls
stay inline. Upload failure does not trigger fallback. Back up the complete
Artifact store and roots with reference-bearing journals.

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

Call `papercut` once for an incidental harness, tooling, environment, confusing,
or suspicious problem, then continue. Do not report non-events or retry failures.
Reports are plaintext in per-instance User state: omit secrets/personal data.
Memory-only mode and storage/shutdown failures can lose reports. Otherwise
active and archived reports stay indefinitely without automatic filing,
redaction, rotation, or deletion.

Operators can review the normal instance with `tau dev papercut list
--markdown`; `tau dev papercut clear` **archives**, rather than deletes,
the active records and prints the archive path. Agent history access is
separately opt-in: set
`extensions.std-utils.config.papercut_history.enable: true` **and** grant
`papercut_list`, `papercut_read`, and/or `papercut_archive` explicitly to
the intended role. Enabling ordinary `papercut` does not grant history.
List returns numbered metadata and an opaque snapshot `generation`. Read accepts
one-based `index` and `expected_generation` and returns one report. Read all listed
reports before passing the generation to archive, which preserves the whole active
file only if its bytes still match. Changed history requires another list. The
generation identifies bytes; it does not prove semantic review. Keep this grant
narrow; the tools inspect shared per-instance records, not just the calling
agent's reports. See
`crates/tau-ext-utils/README.md` for config examples and storage limits.
