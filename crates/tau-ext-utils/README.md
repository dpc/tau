# std-utils

`std-utils` provides the normal `timer` and artifact-backed `read_image` tools,
plus a default-enabled best-effort `papercut` reporter. `read_image(key)` verifies
one immutable original through Artifact RPC, then applies the existing bounded
PNG/JPEG/WebP preparation profiles and returns one typed image tool result. It
does not grant filesystem or shell authority, and Artifact reads do not renew
original retention age.

The reporter's model-visible guidance says: “Use this tool only if you
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


## Disable papercuts

Papercuts are enabled by default for every agent using a configured `std-utils`
instance. Disable them with an explicit per-instance override:

```yaml
extensions:
  std-utils:
    config:
      papercut:
        enable: false
```

By default, Tau declares the model-visible `papercut` tool with one required
`report` string. The override above removes it. The normal global and role tool
policy still applies, so an explicit role allow-list or disable rule can also
hide it. The setting does not bypass that policy.

The separate history tools are **off by default**. Enable them for a
configured utility extension instance with:

```yaml
extensions:
  std-utils:
    config:
      papercut_history:
        enable: true
```

This only declares the history tools; it does not make them visible or callable
for any role. Grant the three exact names to the intended role, for example:

```yaml
agents:
  role_groups:
    coordinator:
      roles:
        coordinator:
          enable_tools: [papercut_list, papercut_read, papercut_archive]
```

Merge these settings into the existing role and instance configuration. Future
roles remain unable to use history without an explicit grant. The separate
`papercut_history` tool group may also be enabled deliberately; enabling the
ordinary `papercut` reporter group does not grant history access. Restrict the
instance opt-in to the intended sandbox: a role grant applies wherever that
configuration is loaded.

`papercut_list` shows only active report count, timestamps, agent IDs, and
session IDs. `papercut_read` shows complete active reports in the same Markdown
format as `tau dev papercut list --markdown`. `papercut_archive` validates
and preserves the whole active file in a numbered private archive, like
`tau dev papercut clear`; it does not delete reports. All three take no
arguments. Each call reads a fresh snapshot, so listing or reading does not
reserve the reports for a later archive. Archive fails rather than moving a
file that changed after validation. They use the authenticated extension
instance's User-scope storage and still obey global and role tool policy.
When the harness is older than protocol 10.1 or does not advertise its
revision, all three tools return an explicit unsupported error without
sending a history request or interrupting other utility tools.


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
reads and atomically renames the canonical JSONL file while holding it, then
reports the number of records removed from the active listing and the preserved
archive path. Archives use the first unused
`papercuts.archive-NNNNNNNNNNNNNNNN.jsonl` name. An append completed before that
lock boundary is preserved in the archive. An append that waits for or starts
after it writes a new active file and remains visible to `list`. The command
does not create storage for an absent reporter, and repeating clear on an empty
history succeeds with a zero count and no new archive.


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

Reports and clear-created archives are plaintext operational notes retained
indefinitely with per-instance extension state; Tau does not enumerate, expire,
or delete archives automatically.
Do not put secrets, credentials, private keys, access tokens, or unnecessary
personal data in a report. Operators who can inspect Tau state can read
papercuts. Older per-session papercut files remain historical artifacts; Tau
does not migrate or merge them automatically.
