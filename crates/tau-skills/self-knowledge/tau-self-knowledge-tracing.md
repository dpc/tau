---
name: tau-self-knowledge-tracing
description: >
  Use this skill when inspecting or filtering durable Tau agent traces, compact
  virtual-tool timelines, content-free performance accounting, trace-relative
  timing, TOON trace output, or JSONL output with jq.
advertise: false
---

# Durable agent tracing

Use the compact TOON projection for an efficient logical overview of an agent's
virtual tool activity:

```console
tau agent trace <agent-id> --include-descendants
```

The command defaults to `--format agent-tools-toon --mode lite`. Add
`--mode full` when complete tool outputs are needed:

```console
tau agent trace <agent-id> --include-descendants \
  --format agent-tools-toon --mode full
```

The TOON document starts with trace metadata and a counted `calls[N]` array.
Important call fields are:

- `at_us`: microseconds from the earliest included durable journal occurrence,
  never an absolute timestamp.
- `duration_us`: declaration-to-terminal duration; absent for incomplete calls
  or a decreasing terminal clock.
- `agent_id`, `call_id`, and `tool`: call identity and owner. A control-bearing
  ID uses `call_id_base64` instead.
- `command`: directly readable command for `shell`, `shell_command`, and the
  internal `gpt_shell` surface.
- `arguments`: complete ordinary JSON-shaped arguments.
- `arguments_json_base64`: replaces only `arguments` when tagged-CBOR, exact
  floats, or unsafe controls cannot be represented directly by TOON; Base64-decode
  it and parse the result as JSON. Rare control-bearing command/output text uses
  `command_base64`/`output_base64` while the call envelope stays readable.
- `status`: `ok`, `error`, `cancelled`, or `incomplete`.
- Terminal calls always emit `output_bytes` and `output_lines` for their complete
  rendered projection. Lite includes at most its first 4 KiB; full includes all
  of it. `output_complete` distinguishes complete from clipped lite output.
  Control-bearing text uses `output_base64`. Incomplete calls omit output and
  counts and set `output_complete: false`.

Calls are projected by journal wall clock with deterministic ties. Cross-agent
order is not causal. TOON direct output escapes embedded newlines as `\n` inside
a quoted scalar, so each call stays structurally framed in the counted array.

Use JSON Lines when shell pipelines need targeted selection:

```console
# Shell calls with their relative time and command.
tau agent trace <agent-id> --include-descendants \
  --format agent-tools-jsonl |
  jq -c 'select(.record_type == "call" and
                (.tool == "shell" or .tool == "shell_command" or .tool == "gpt_shell")) |
         {at_us, agent_id, status, command, output_bytes, output_lines}'

# Failures and incomplete calls.
tau agent trace <agent-id> --include-descendants \
  --format agent-tools-jsonl |
  jq -c 'select(.record_type == "call" and .status != "ok")'

# Full outputs from one tool.
tau agent trace <agent-id> --format agent-tools-jsonl --mode full |
  jq -r 'select(.record_type == "call" and .tool == "read") |
         [.at_us, .call_id, .output] | @tsv'
```

Use the content-free performance projection for provider-reported token/cache
accounting, harness-calculated cost, and qualified prompt lifecycle intervals:

```console
tau agent trace <agent-id> --include-descendants \
  --format agent-performance-jsonl
```

It emits one row per materialized ordinary-inference prompt and excludes
standalone compaction and terminal-only facts. Each row includes the
provider-qualified model ID, and each included agent gets one summary. Missing
usage and cost remain absent and are counted separately. Cache
ratios use integer parts per million. `recorded_at_wall_elapsed_us` measures
only the wall-clock interval between journal append invocations; it is not
provider wire latency, durable commit time, or exact execution time. The
projection never includes prompt, tool, response, or error bodies. Treat model
IDs as sensitive metadata.

Both compact agent-tool formats include only provider-declared, model-visible tool calls;
they omit assistant prose and lower-level lifecycle facts. `tau-jsonl` remains
the complete canonical durable artifact, while `otlp-json` remains the telemetry
adapter.

Treat every trace as sensitive. Both compact agent-tool formats expose unredacted tool
names, arguments, and commands; full mode also exposes complete outputs,
including rendered error details.
Projection uses anonymous payload staging, but heap still grows with call count
and encoded call-ID/tool-name bytes. Ambiguous simultaneous background
generations with the same call ID fail export rather than producing false
correlation.
