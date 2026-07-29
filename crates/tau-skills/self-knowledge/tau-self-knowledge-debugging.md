---
name: tau-self-knowledge-debugging
description: >
  Use this skill when debugging Tau sessions, daemons, runtime behavior, socket
  attachment, replay, logs, provider requests, token/cache usage, event ordering,
  or persisted state under Tau config, state, session, and runtime directories.
advertise: false
---

For context-window failures, distinguish the provider's typed rejection from
the harness recovery decision. Providers cannot authorize reactive compaction;
inspect the durable inference checkpoint, model capability, role policy,
activation cut, branch correlation, and whether streamed/final semantic output
made recovery unsafe.

## Important paths

Tau follows the XDG directories:

- Config: `~/.config/tau/`
  - `cli.yaml`, `cli.d/*.yaml` — CLI display and key-binding config.
  - `harness.yaml`, `harness.d/*.yaml` — harness, agent roles/defaults, extension, and session-retention config.
- State: `~/.local/state/tau/` on Linux.
  - If no XDG state dir is available, inspection defaults may fall back to `.tau/state`.
  - `cli.json` — persisted CLI runtime toggles such as show-diff, show-thinking, show-tools, turn stats.
  - `auth.d/<provider>.json` — per-provider credentials.
  - `auth.json` — legacy whole-file credentials, read for backwards compatibility.
- Sessions: `~/.local/state/tau/sessions/<session_id>/`
  - `events.cbor` — durable per-session membership journal (`session.agent_loaded` / `session.agent_unloaded`).
  - `meta.json` — session metadata such as creation time and last-touched time.
  - `lock` — flock used while the daemon has the session loaded for writing.
  - `events.jsonl` — best-effort debug runtime event log. It is an ordered subsequence of attempted observations, not authoritative replay state; a missing row does not prove an event was absent.
  - `debug/provider-requests/*-{request,response}.json.zst` — zstd-compressed exact upstream provider request bodies plus parsed response captures written best-effort by provider extensions only when the harness reports the current session as durable and the durable session directory already exists, keyed by timestamp, `agent_prompt_id`, and transport. Tau does not intentionally serialize auth headers or API-key configuration, but these records contain full prompt/tool/model content and provider-controlled responses/errors or configured request fields can reflect credentials. Treat every capture as potentially credential-bearing. Use `zstdcat` or `zstd -dc` before `jq`; legacy uncompressed `.json` captures may remain until retention cleanup. Queue overload, write failure, or process exit can omit a capture or leave a truncated final stream, so decompression failure is not authoritative evidence about provider activity.
  - `logs/tau-harness.log` — harness daemon stderr/tracing for the session.
  - `logs/<extension>.log` — stderr for each spawned extension.
- Agents: `~/.local/state/tau/agents/<agent_id>/`
  - `events.cbor` — durable agent transcript log and source of truth for replaying that agent tree.
  - `meta.json` — agent metadata such as cwd, creation time, and latest prompt preview.
  - `lock` — flock used while the daemon has the agent loaded for writing.
- Runtime: `${XDG_RUNTIME_DIR}/tau/harnesses/` or `/tmp/tau-$USER/harnesses/`
  - `<pid>-<instance>.sock` — Unix socket for clients.
  - `<pid>-<instance>.json` — daemon discovery metadata containing pid, project root,
    Tau version, and the daemon's current active `session_id`; `:session new`
    updates this field after the daemon switches sessions successfully.

## Event logs are usually the first place to look

For session misbehavior, inspect `~/.local/state/tau/sessions/<session_id>/events.jsonl` early. It is append-only JSONL meant for post-mortems and may include transient observations absent from durable replay. It is useful for missing UI updates, streaming, tool progress, connection churn, ordering, and short-lived states, but it is deliberately incomplete.

Producers redact and serialize each line, then attempt immediate nonblocking
admission to one process-wide FIFO bounded at 1,024 retained lines and 64 MiB
including in-flight line and path bytes. A detached worker takes
`<session>/events.jsonl.lock` per line, appends at exact EOF, and flushes without
fsync. Queue overflow, lock/open/write failure, uncertain rollback, and
nonjoining process exit can omit rows; OS-cache loss or a torn final line is
also possible. The worker never delays semantic event handling or fsyncs. No
shutdown path requests or waits for a drain or joins
the worker; it may continue draining while the process remains alive, and exit
may interrupt queued or in-flight work. Restart does not repair a torn tail.

Each debug log line includes fields such as:

- `type` — commonly `from_connection`, `published`, `disconnected`, or `new_client`.
- `recorded_at_micros` — timestamp useful for ordering and latency gaps.
- `source` — connection id when known.
- `event_name` — protocol event name.
- `event` — compacted event payload.

Use session `events.cbor` when debugging membership replay, and agent `events.cbor` when debugging transcript/tree reconstruction. Use `events.jsonl` when debugging runtime behavior.

## Drive a running session

Use `cargo r -- dev send <session_id> <line...>` to inject user-equivalent input into a running daemon-bound session. This is useful for agent-powered debugging because it goes through the socket protocol and normal UI event path instead of editing persisted logs by hand.

Examples:

```bash
cargo r -- dev send <session_id> "normal user message"
cargo r -- dev send <session_id> :cancel
cargo r -- dev send <session_id> :model test/model
cargo r -- dev send <session_id> :compact
cargo r -- dev send <session_id> '!pwd'
```

The command requires the session id and finds the matching running daemon via
runtime harness metadata. That metadata's `session_id` is the daemon's active
current session and is updated by `:session new`; discovery leaves stale runtime
files untouched, and ambiguous live matches are treated as an error. It
supports normal prompts, core commands, and `!` / `!!` shell-command
submissions.

## Quick inspection workflow

1. Identify a running session with `tau session list`; use `--dir DIR` for an
   exact canonical project-root filter or `--json` for structured session/root
   records. Relative filters resolve from caller CWD, and invalid filters exit 2
   rather than looking like an empty live result. For historical sessions,
   inspect `~/.local/state/tau/sessions/` and sort by `meta.json` or directory
   mtime.
2. Read `events.jsonl` around the failing prompt first.
3. Cross-check with `logs/tau-harness.log` and extension logs for errors or panics.
4. Check session/agent `events.cbor` only when the bug involves replay or persisted semantic contents.
5. Check runtime daemon files under `${XDG_RUNTIME_DIR}/tau/` when the bug involves attach/resume, wrong project daemon selection, or socket connection failures.
6. For provider/cache-shape bugs, inspect `debug/provider-requests/` for the exact request body Tau sent upstream and the response capture it parsed afterward.

Helpful commands:

```bash
# Pretty-print recent debug events for one session.
tail -n 200 ~/.local/state/tau/sessions/<session_id>/events.jsonl | jq .

# Find recent session directories.
find ~/.local/state/tau/sessions -maxdepth 1 -mindepth 1 -type d -printf '%T@ %p\n' | sort -n

# Inspect logs for one session.
ls -lah ~/.local/state/tau/sessions/<session_id>/logs

# Inspect exact provider request/response captures, if present.
ls -lah ~/.local/state/tau/sessions/<session_id>/debug/provider-requests
# Responses-backend request/response fields.
zstdcat ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*-sp-6-*-request.json.zst | jq 'select(.backend == "responses") | .body.previous_response_id, .body.input'
zstdcat ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*-sp-6-*-response.json.zst | jq 'select(.backend.kind == "responses" or .backend == "responses") | .provider_response_id, .usage, .provider_response_finished.output_items, .provider_terminal_event'

# Chat Completions request, successful-response, and HTTP-error fields.
zstdcat ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*-sp-6-*-request.json.zst | jq 'select(.backend == "chat_completions") | .body.messages'
zstdcat ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*-sp-6-*-response.json.zst | jq 'select(.backend == "chat_completions") | .usage, .output_items, .raw_events, .http_status, .body'
```


## Token/cache efficiency analysis

When asked to analyze cache hit or token usage efficiency for a session, inspect
`events.jsonl`. Raw Provider input uses
`provider.response_finished_reported`; the enriched harness-canonical published record
uses `provider.response_finished`. Prefer canonical `type: "published"` records for
harness-derived usage, or select one name/type explicitly rather than combining both.

Useful one-shot summary:

```bash
python3 - <<'PY'
import json, pathlib
sid = '<session_id>'
p = pathlib.Path.home() / '.local/state/tau/sessions' / sid / 'events.jsonl'
rows = []
for ln, line in enumerate(p.open(), 1):
    j = json.loads(line)
    ev = j.get('event', {})
    if ev.get('event') == 'provider.response_finished' and j.get('type') == 'published':
        pl = ev.get('payload', {})
        usage = pl.get('usage') or {}
        sp = pl.get('agent_prompt_id') or '?'
        inp = usage.get('prompt_sent_tokens') or pl.get('input_tokens') or 0
        cached = usage.get('prompt_cached_tokens') or pl.get('cached_tokens') or 0
        out = usage.get('response_received_tokens') or pl.get('output_tokens') or 0
        rows.append((sp, ln, inp, cached, inp - cached, out, pl.get('originator')))

for label, subset in [('all', rows), ('user', [r for r in rows if (r[6] or {}).get('kind') == 'user']), ('extension', [r for r in rows if (r[6] or {}).get('kind') == 'extension'])]:
    total_in = sum(r[2] for r in subset)
    total_cached = sum(r[3] for r in subset)
    total_uncached = sum(r[4] for r in subset)
    total_out = sum(r[5] for r in subset)
    pct = 100 * total_cached / total_in if total_in else 0
    print(label, 'calls', len(subset), 'input', total_in, 'cached', total_cached, 'uncached', total_uncached, 'cache_pct', round(pct, 1), 'output', total_out)

print('\nlargest uncached calls:')
for sp, ln, inp, cached, uncached, out, origin in sorted(rows, key=lambda r: r[4], reverse=True)[:10]:
    pct = 100 * cached / inp if inp else 0
    print(sp, 'line', ln, 'input', inp, 'cached', cached, 'uncached', uncached, 'cache_pct', round(pct, 1), 'output', out, 'origin', origin)
PY
```

Red flags found in past sessions:

- Internal extension prompts, especially `std-notifications` idle summaries, can create normal `ui.prompt_submitted` / `agent.prompt_created` / `provider.prompt_submitted` sequences with originator `{kind: "extension"}`. If they resend full history, cache continuity may collapse and waste many uncached tokens for tiny outputs. Check lines around `agent.start_request`, `ui.prompt_submitted`, and the following `provider.response_finished`.
- `harness.context_usage_changed` currently follows all `provider.response_finished` events, including extension-originated prompts. Treat context/token stats carefully if side-channel prompts are present.
- Large tool outputs in `agent.prompt_created` messages can dominate context: repeated large `read` slices, cargo/check output, clippy output, or colorized `jj diff`. Grep for `┄total <n>┄` markers in `events.jsonl` to find compacted large payloads.
- For exact, uncompacted provider payloads, check `debug/provider-requests/*-{request,response}.json.zst` with `zstdcat`; legacy `.json` captures are also readable directly. Request files are especially useful for cache misses involving `previous_response_id`, multi-tool-call suffixes, tool-use/tool-result ordering, or mismatches between `agent.prompt_created` and the serialized upstream `body.input`; response files show Tau's parsed `provider.response_finished` shape plus the raw terminal provider event (`response.completed` / `response.done`) when available.
- Repeated `provider.response_updated` streaming events are numerous and not useful for aggregate token accounting. Prefer `provider.response_finished`.

Quick checks for side-channel waste:

```bash
# Show extension-originated prompt/response activity.
grep -n 'agent.start_request\|std-notifications\|"kind":"extension"' ~/.local/state/tau/sessions/<session_id>/events.jsonl

# Search logs for runtime errors; no matches does not rule out token waste.
grep -RniE 'error|warn|panic|cache|token' ~/.local/state/tau/sessions/<session_id>/logs
```

## Debug watcher-visible provider status

`agent_watch` reports only sanitized retry/work categories, saturating attempts,
and approximate delays. Enabling or re-enabling shows the current snapshot rather
than attempt history. Raw endpoint bodies and provider error text intentionally
remain unavailable across the watch boundary; inspect provider-local debug logs
under the existing diagnostics policy when those details are required.
### Debugging manual compaction

Trace `agent.manual_compaction_requested` by request id, then expect exactly
one pre-start failure or a matching `agent.standalone_compaction_started`.
After transaction success/failure, verify one terminal background event for
the initiating call. For self requests, the start cut must include the complete
sibling tool-result round; a request without that boundary remains deferred.
