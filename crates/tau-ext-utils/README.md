# std-utils

`std-utils` provides the normal `timer` tool and an opt-in best-effort
`papercut` reporter. Its model-visible guidance says: “Use this tool only if you
encounter an incidental Tau harness, tooling, environment, confusing, or suspicious
problem. Record one concise, best-effort report, then continue the primary task. Do
not call it merely to state that no problem occurred, and do not retry.”


## Daily timers

Relative timers continue to use `delay_seconds` and optional
`interval_seconds`. A daily timer uses an exact wall-clock time instead:

```json
{"action":"schedule","timer_id":"agenda","daily_time":"08:00","message":"prepare today's agenda"}
```

The default follows the running host's local timezone, including daylight-saving
changes. Set `"utc": true` to use UTC. A spring-forward date that has no requested
local time is skipped; a repeated fall-back time fires once at its earlier
occurrence. The first firing is strictly after scheduling. Downtime coalesces all
overdue occurrences into one wakeup with an exact count.

Tau reconstructs daily timers from the existing session replay facts. Restart
uses the host's then-current local configuration and rules. While local timers are
active, a running Tau process polls Jiff's system-timezone discovery on a
60-second monotonic cadence. Jiff caches system discovery for approximately five
minutes, so a changed host configuration can take that long to affect timers. A
refresh never replaces an already-due occurrence. A transient lookup failure
retains accepted restored timers for a later refresh.

Each timer has one daily time, so register separate timer IDs for separate times
and cancel or list them through the existing session-scoped actions.


## Enable papercuts

Papercuts are disabled by default. Enable them for every agent using one
configured `std-utils` instance:

```yaml
extensions:
  std-utils:
    config:
      papercut:
        enable: true
```

This declares the model-visible `papercut` tool with one required `report`
string. The normal global and role tool policy still applies, so an explicit
role allow-list or disable rule can hide it. The setting does not bypass that
policy.


## Records

Each accepted call appends exactly one compact JSONL line to

```text
$XDG_STATE_HOME/tau/ext/<std-utils-instance>/papercuts.jsonl
```

This is `ExtensionDataScope::User`: the harness chooses the authenticated
configured instance directory. The normal `std-utils` instance therefore uses
`$XDG_STATE_HOME/tau/ext/std-utils/papercuts.jsonl`. `std-utils` owns only the
relative `papercuts.jsonl` filename and record content.

The stable v1 line schema is:

```json
{"schema":1,"agent_id":"engineer-abc","session_id":"20250101-...","timestamp_us":1735689600000000,"report":"tool output omitted a useful diagnostic"}
```

`agent_id` comes from harness-routed `tool.started`; `session_id` comes from
the current harness-authored `session.started`; neither comes from model
arguments. `timestamp_us` is the operation wall-clock Unix-microsecond time.

Inspect this configured instance's shared records in batches with standard JSONL
tools:

```sh
jq -c . "$XDG_STATE_HOME/tau/ext/std-utils/papercuts.jsonl"
jq -c 'select(.schema == 1)' "$XDG_STATE_HOME/tau/ext/std-utils/papercuts.jsonl"
```

For the normal `std-utils` instance, Tau also provides concise operator-facing
inspection:

```sh
tau dev papercut list
tau dev papercut list --markdown
tau dev papercut clear
```

Each command accepts `--state-dir DIR`, which defaults to Tau's normal state
directory. Plain list output escapes control characters into one line per
report; Markdown retains report line boundaries inside a literal code block.
Both sort the same v1 records by timestamp, agent, session, and report.

`clear` takes the reporter's existing per-instance extension-directory lock,
reads and removes the canonical JSONL file while holding it, then reports the
number of records removed. An append completed before that lock boundary is
cleared. An append that waits for or starts after it writes a new file and is
preserved. The command does not create storage for an absent reporter and
repeating clear on an empty history succeeds with a zero count.


## Limits and behavior

`report` must contain non-whitespace text, at most 4,096 Unicode scalar values
and 16 KiB UTF-8 bytes. The harness retains the whole per-instance file without
rotation, retry, deduplication, redaction, upload, issue filing, or replay
re-append. Its existing extension-data limit caps the resulting file at 16 MiB.

One accepted call makes one `AppendFile` RPC and writes one trailing-newline
record. The harness serializes User-scope appends across harness processes that
share this Tau state root and configured instance, then synchronously
`sync_all`s the file. Papercuts are best-effort and non-transactional:
memory-only mode, a full file, an RPC failure, and a final-shutdown timing
race can leave a report unrecorded. Ephemeral sessions use the same
durable per-instance file. The tool returns a concise recorded/not-recorded outcome
and tells the agent to continue its primary task without retrying.

Reports are plaintext operational notes retained with per-instance extension state.
Do not put secrets, credentials, private keys, access tokens, or unnecessary
personal data in a report. Operators who can inspect Tau state can read
papercuts. Older per-session papercut files remain historical artifacts; Tau
does not migrate or merge them automatically.
