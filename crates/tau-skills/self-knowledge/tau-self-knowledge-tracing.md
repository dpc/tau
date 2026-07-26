---
name: tau-self-knowledge-tracing
description: >
  Use this skill when inspecting or filtering durable Tau agent traces, compact
  virtual-tool timelines, trace-relative timing, TOON trace output, or JSONL
  agent-tool output with jq.
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
- Lite mode emits `output_bytes` and `output_lines`; full mode normally emits
  `output`, with `output_base64` for control-bearing text.

Calls are projected by journal wall clock with deterministic ties. Cross-agent
order is not causal. TOON full output escapes embedded newlines as `\n` inside a
quoted scalar, so each call stays structurally framed in the counted array.

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

Both compact formats include only provider-declared, model-visible tool calls;
they omit assistant prose and lower-level lifecycle facts. `tau-jsonl` remains
the complete canonical durable artifact, while `otlp-json` remains the telemetry
adapter.

Treat every trace as sensitive. Both compact formats expose unredacted tool
names, arguments, and commands; full mode also exposes complete outputs,
including rendered error details.
Projection uses anonymous payload staging, but heap still grows with call count
and encoded call-ID/tool-name bytes. Ambiguous simultaneous background
generations with the same call ID fail export rather than producing false
correlation.
