---
name: tau-self-knowledge-tracing
description: Inspect durable Tau agent traces and compact semantic/correlation projections.
---

# Durable agent tracing

Export the compact projection with:

```console
tau agent trace <agent-id> --include-descendants --format agent-tools-toon
tau agent trace <agent-id> --include-descendants --format agent-tools-jsonl | jq -c .
```

Both formats use `tau.agent_trace_compact`, schema version `0`, and contain
provider-declared `call`, assistant prose/reasoning, explicit directional
messages, content-free `activation`, and typed `relationship` items. Every item
includes relative append time, optional absolute Unix append time, and owning
journal sequence. Treat only explicit observation/call/message references as
correlation or causal evidence; wall-clock order and adjacency across agents are
not causal evidence. Missing selected facts remain `source_not_selected`,
`unresolved`, or `incomplete`.

Qualified `*_us` intervals exist only when both explicitly linked endpoints are
selected and nondecreasing. There is no generic `duration_us`.
Completion-delivering waits point to the terminal owner through `output_ref`;
they never copy owner output. Lite bounds each semantic text/output item at
4 KiB while retaining complete `text_bytes`/`text_lines` and
`output_bytes`/`output_lines`; full retains complete content.

Treat every trace as sensitive: compact formats expose unredacted assistant
prose, displayable reasoning, explicit messages, tool arguments/output, exact
timestamps, and identity/activity metadata. Captured identity/timestamps remain
historical journal facts when journals move between sessions.

For content-free prompt and usage accounting, use:

```console
tau agent trace <agent-id> --format agent-performance-jsonl
```
