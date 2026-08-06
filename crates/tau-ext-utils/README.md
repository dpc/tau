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
$XDG_STATE_HOME/tau/sessions/<session-id>/ext/data/<std-utils-instance>/papercuts.jsonl
```

This is `ExtensionDataScope::Session`: the harness chooses the authenticated
configured instance directory and current session root. `std-utils` owns only
the relative `papercuts.jsonl` filename and record content.

The stable v1 line schema is:

```json
{"schema":1,"agent_id":"engineer-abc","session_id":"20250101-...","timestamp_us":1735689600000000,"report":"tool output omitted a useful diagnostic"}
```

`agent_id` comes from harness-routed `tool.started`; `session_id` comes from
the current harness-authored `session.started`; neither comes from model
arguments. `timestamp_us` is the operation wall-clock Unix-microsecond time.

Inspect a session's records in batches with standard JSONL tools:

```sh
jq -c . "$XDG_STATE_HOME/tau/sessions/$session_id/ext/data/std-utils/papercuts.jsonl"
jq -c 'select(.schema == 1)' "$XDG_STATE_HOME/tau/sessions/$session_id/ext/data/std-utils/papercuts.jsonl"
```


## Limits and behavior

`report` must contain non-whitespace text, at most 4,096 Unicode scalar values
and 16 KiB UTF-8 bytes. The harness retains the whole session file without
rotation, retry, deduplication, redaction, upload, issue filing, or replay
re-append. Its existing extension-data limit caps the resulting file at 16 MiB.

One accepted call makes one `AppendFile` RPC and writes one trailing-newline
record. The harness preserves its current normal single-writer order and
synchronous `sync_all` behavior. Papercuts are best-effort and
non-transactional: memory-only or ephemeral sessions, a full file, an RPC
failure, and the rare session rollover timing mismatch can leave a report
unrecorded. The tool returns a concise recorded/not-recorded outcome and tells
the agent to continue its primary task without retrying.

Reports are plaintext operational notes retained with their session. Do not put
secrets, credentials, private keys, access tokens, or unnecessary personal data
in a report. Operators who can inspect the session state can read papercuts.
