# std-utils

`std-utils` provides the normal `timer` tool and an opt-in best-effort
`papercut` reporter. The reporter lets an agent leave one concise note about an
incidental Tau harness, tooling, environment, confusing, or suspicious problem
without redirecting its primary task.


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
memory-only mode, a full file, an RPC failure, and the rare session rollover
timing mismatch can leave a report unrecorded. Ephemeral sessions use the same
durable per-instance file. The tool returns a concise recorded/not-recorded outcome
and tells the agent to continue its primary task without retrying.

Reports are plaintext operational notes retained with per-instance extension state.
Do not put secrets, credentials, private keys, access tokens, or unnecessary
personal data in a report. Operators who can inspect Tau state can read
papercuts. Older per-session papercut files remain historical artifacts; Tau
does not migrate or merge them automatically.
