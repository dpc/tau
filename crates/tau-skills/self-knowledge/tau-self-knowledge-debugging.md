---
name: tau-self-knowledge-debugging
description: Use when debugging Tau sessions, daemons, runtime behavior, socket attachment, replay, logs, provider requests, token/cache usage, event ordering, or persisted config, state, session, and runtime data.
advertise: false
---

## Important paths

Tau follows XDG directories. Linux defaults:

- Config: `~/.config/tau/`
  - `cli.yaml`, `cli.d/*.yaml`: display and key bindings.
  - `harness.yaml`, `harness.d/*.yaml`: roles, extensions, and retention.
- State: `~/.local/state/tau/`; inspection may fall back to `.tau/state` without
  an XDG state directory.
  - `cli.json`: persisted UI toggles.
  - `providers/<extension>/<provider>.json`: provider settings.
  - `secrets/ext/<extension>/providers/<provider>/`: typed credentials.
- Sessions: `~/.local/state/tau/sessions/<session_id>/`
  - `events.cbor`: durable membership (`session.agent_loaded` /
    `session.agent_unloaded`).
  - `meta.json`: session existence, creation time, and derived last-touched hint.
  - `lock`: flock while loaded for writing.
  - `events.jsonl`: best-effort runtime observations, not replay authority.
  - `debug/provider-requests/<provider-instance>/*.json.zst`: private captures.
  - `logs/tau-harness.log`, `logs/<extension>.log`: harness/extension stderr.
- Agents: `~/.local/state/tau/agents/<agent_id>/`
  - `events.cbor`: authoritative transcript/tree replay.
  - `meta.json`: cwd, creation time, latest prompt preview.
  - `lock`: flock while loaded for writing.
- Runtime: `${XDG_RUNTIME_DIR}/tau/harnesses/` or `/tmp/tau-$USER/harnesses/`
  - `sockets/<session-key>.sock`: one immutable session's socket.
  - `claims/<session-key>.lock`: lifetime flock. Its JSON and PID are diagnostic;
    the full BLAKE3 session key determines routing.

## Runtime discovery from supervised components

The default `tau_runtime_socket_access: hidden` gives shell/extension processes
an empty, read-only `${XDG_RUNTIME_DIR}/tau/harnesses` mount. Thus
`tau session list --json` can return `[]` while host listeners are live.
Discover daemons from the host. Use `tau_runtime_socket_access: ambient` only
for trusted components that need it; do not change the default.
See [extension configuration](../../../docs/extensions.md#runtime-socket-discovery)
and the [configured-extension boundary](../../../SECURITY.md).

Check namespace masking before diagnosing an unlinked socket or resume failure:

```bash
printf 'XDG_RUNTIME_DIR=%s\n' "$XDG_RUNTIME_DIR"
readlink /proc/self/ns/mnt
findmnt -T "$XDG_RUNTIME_DIR/tau/harnesses"
find "$XDG_RUNTIME_DIR/tau/harnesses" -maxdepth 1 -printf '%y %f\n'
ss -xlpn | grep '/tau/harnesses/'
```

`ss` pathnames plus an empty read-only bind mount indicate masking, not unlink.

## Inspection workflow

1. Use `tau session list`; `--dir DIR` filters by exact canonical project root
   and `--json` returns structured records. Relative filters resolve from caller
   CWD; invalid filters exit 2. For historical sessions, inspect state directories
   and their `meta.json` or mtimes.
2. Read `events.jsonl` around the failure for UI updates, streaming, tools, and
   transient ordering; cross-check harness and extension logs.
3. Use session `events.cbor` for membership replay and agent `events.cbor` for
   transcript/tree reconstruction. Check runtime claims/sockets for attach failures.
4. Use private provider captures for exact wire shape, and
   `tau-self-knowledge-cache` for durable usage/cache accounting.

`events.jsonl` can omit rows or end with a torn line after queue pressure, I/O
failure, or exit. Shutdown does not drain/fsync it; restart does not repair it.
Absence proves nothing about whether an event occurred. Useful fields are
`type` (`from_connection`, `published`, `disconnected`, `new_client`),
`recorded_at_micros`, `source`, `event_name`, and `event`.

```bash
tail -n 200 ~/.local/state/tau/sessions/<session_id>/events.jsonl | jq .
find ~/.local/state/tau/sessions -maxdepth 1 -mindepth 1 -type d -printf '%T@ %p\n' | sort -n
ls -lah ~/.local/state/tau/sessions/<session_id>/logs
```

Without a valid `TAU_LOG`, built-in components log their own target at `info`
and others at `warn`; custom extensions choose their fallback. A valid value
replaces the filter. Set it when starting/restarting the harness, not on a later
`tau attach`. Load `tau-self-knowledge-debugging-extensions` for extension
startup, reconnects, raw stderr privacy, and the lossy service-stderr mirror.

## Drive a running session

Use `cargo r -- dev send <session_id> <line...>` for user-equivalent input
through the running session's socket/UI path:

```bash
cargo r -- dev send <session_id> "normal user message"
cargo r -- dev send <session_id> :cancel
cargo r -- dev send <session_id> :model test/model
cargo r -- dev send <session_id> '!pwd'
```

The command verifies session identity and supports prompts, core commands, and
`!` / `!!` shell submissions. It rejects `dev send <session_id> :compact`
because it has no agent target; another UI's selection does not supply one.
Attach, select the agent, and enter `:compact`, or use enabled model tools
`compact {}` / `agent_compact {agent_id}`. See
[manual compaction tools](tau-self-knowledge-harness.md#manual-compaction-tools).

## Provider captures and cache evidence

Use `zstdcat` or `zstd -dc` before `jq`. Request/response captures can contain
full prompts, tools, model output, provider text, and credentials.
`responses-attempt-failure` records omit prose/raw values but retain bounded
shape, lengths, and validated IDs/codes; still keep them private.
Missing/truncated captures and decompression errors do not prove absence of
provider activity.

Match Responses inference records by `agent_prompt_id`, `logical_attempt`, and
`wire_dispatch_index`; transparent repair increments the dispatch index within
one attempt. Unary compaction omits inference correlation.

```bash
find ~/.local/state/tau/sessions/<session_id>/debug/provider-requests -maxdepth 2 -type f -print
# Responses request.
zstdcat ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*/*-sp-6-*-request.json.zst | jq 'select(.backend == "responses") | .body.previous_response_id, .body.input'
# Synthetic terminal projection; absent record_kind admits older records.
zstdcat ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*/*-sp-6-*-response.json.zst | jq 'select(.record_kind == "provider_response_finished" or .record_kind == null) | select(.backend == "responses" or .backend.kind? == "responses") | .provider_response_id, .usage, .provider_response_finished.output_items, .provider_terminal_event'
# Exact Codex received text, including rejected events; not a terminal report.
zstdcat ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*/*-sp-6-websocket-response.json.zst | jq 'select(.record_kind == "received_response") | {logical_attempt, wire_dispatch_index, attempt_id, raw_events, prefix_truncated, last_received_event, last_received_event_index, last_event_truncated, accepted_terminal, capture_complete, error, error_truncated, decode_error}'
# Bounded failed-attempt evidence.
zstdcat ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*/*-sp-6-responses-attempt-failure.json.zst | jq '{logical_attempt, wire_dispatch_index, classification, wire, provider, transport, truncation}'
# Chat Completions request and response/error fields.
zstdcat ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*/*-sp-6-*-request.json.zst | jq 'select(.backend == "chat_completions") | .body.messages'
zstdcat ~/.local/state/tau/sessions/<session_id>/debug/provider-requests/*/*-sp-6-*-response.json.zst | jq 'select(.backend == "chat_completions") | .usage, .output_items, .raw_events, .http_status, .body'
```

Codex `received_response` captures retain text before decoding/validation.
After the 1-MiB / 4,096-event prefix fills, `prefix_truncated` marks a gap and
`last_received_event` retains the latest message with its one-based index and
truncation flag. `accepted_terminal` means parser acceptance, not durable
compaction success. See `docs/providers.md` for transport exclusions/privacy.
`agent_watch` exposes sanitized current retry/work status, not attempt history
or raw provider errors; inspect provider-local logs for those.

For durable accounting, use `tau agent cache AGENT` / `tau session cache SESSION`.
For transient event analysis, select canonical `type: "published"` records named
`provider.response_finished`; do not combine them with incoming
`provider.response_finished_reported` or count streaming
`provider.response_updated` as responses.

Extension prompts (`originator: {kind: "extension"}`) can resend history and
affect `harness.context_usage_changed`. Inspect `agent.start_request`,
`ui.prompt_submitted`, and the following response. Look for `┄total <n>┄`
markers for large tool payloads. Exact captures help explain
`previous_response_id`, tool ordering, or serialized `body.input` differences.

```bash
grep -n 'agent.start_request\|std-notifications\|"kind":"extension"' ~/.local/state/tau/sessions/<session_id>/events.jsonl
grep -RniE 'error|warn|panic|cache|token' ~/.local/state/tau/sessions/<session_id>/logs
```

## Interactive prompt latency traces

These content-free operational observations use process-local monotonic timing,
not cross-process/restart causality, protocol identity, or replay authority.

- `tau_cli::prompt_submission`: input, history admission, frame construction.
- `tau_cli::frontend_progress`: writer wait, write, flush, and hold times.
- `tau_cli_term_raw::frontend_progress`: redraw, layout, lock, and flush timing.
  A flush does not prove terminal receipt/paint. Enable
  `tau_cli_term_raw::frontend_progress=trace` in `TAU_LOG`; interactive records
  use that UI's `ui.log`.
- `tau_harness::prompt_ingress`: UI prompt/agent-creation ingress. Socket wait
  includes idle time; wake timing includes the sender/wake tail.
- `tau_harness::prompt_acceptance`: TRACE precursors and an INFO aggregate for
  immediate authenticated, non-internal `HumanUi` prompt dispatch, excluding
  queued prompts, agent creation, and internal/replay/message-wake traffic.
  `result_class` is `publication_accepted`, `semantic_admission_rejected`, or
  `stale_or_torn_down`. Debug-record admission measures synchronous admission,
  not detached filesystem writes; `residual_us` is unclassified elapsed time.

For default-off `tau_cli::delivery_memory` / `tau_harness::delivery_memory`
TRACE targets, see
[`docs/decoded-delivery-memory-measurement.md`](../../../docs/decoded-delivery-memory-measurement.md)
for invocation, outputs, privacy, ownership cuts, and estimator limits.

## Compaction failures

A provider rejection does not authorize reactive compaction. Inspect the durable
inference checkpoint, model/role policy, activation cut, branch correlation, and
any semantic output already delivered.

For manual requests, trace `agent.manual_compaction_requested` by request ID:
expect one pre-start failure or matching `agent.standalone_compaction_started`,
then one terminal background event after transaction success/failure.
Self-compaction must include the complete sibling tool-result round; without
that boundary it remains deferred.
