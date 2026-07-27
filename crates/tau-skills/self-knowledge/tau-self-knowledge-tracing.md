---
name: tau-self-knowledge-tracing
description: Inspect durable Tau agent traces and compact explicit-observation tool projections.
---

# Durable agent tracing

Export the compact projection with:

```console
tau agent trace <agent-id> --include-descendants --format agent-tools-toon
tau agent trace <agent-id> --include-descendants --format agent-tools-jsonl | jq -c .
```

Both formats use `tau.agent_tools`, schema version `0`, and contain `call`, content-free `activation`, and content-free `relationship` records. The header states `timing_basis: producer_wall_clock_at_observation` and `causality: explicit_observation_refs_only`. Treat only explicit `ObservationId` and `ToolCallRef` links as causal; timestamps and journal adjacency are not causal evidence. Missing selected facts remain `source_not_selected`, `unresolved`, or `incomplete`.

Qualified `*_us` intervals exist only when both explicitly linked endpoints are selected and nondecreasing. There is no generic `duration_us`. Completion-delivering waits point to the terminal owner through `output_ref`; they never copy owner `output_bytes`, `output_lines`, or payload. Lite bounds owner output; full retains it.

Treat every trace as sensitive: compact formats expose unredacted tool names, arguments, commands, and owner output.

For content-free prompt and usage accounting, use:

```console
tau agent trace <agent-id> --format agent-performance-jsonl
```
