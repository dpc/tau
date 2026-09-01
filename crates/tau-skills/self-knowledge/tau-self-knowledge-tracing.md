---
name: tau-self-knowledge-tracing
description: Audit Tau agent execution, performance, orchestration, and durable semantic traces.
---

# Durable agent tracing

Choose the least sensitive projection that answers the question:

```console
tau agent trace <agent-id> --include-descendants --format agent-performance-jsonl
tau agent trace <agent-id> --include-descendants --format agent-tools-toon
tau agent trace <agent-id> --include-descendants --format tau-jsonl
```

Use `agent-performance-jsonl` by default. It is content-free and covers ordinary
provider usage/cost, tool/background lifecycle, typed waits and effective input
timeouts, outer turns, and standalone compaction attempts. It still exposes
identities, models, activity timing, usage, cost, membership, and work patterns.

Use `agent-tools-toon` or `agent-tools-jsonl` only for bounded semantic
explanation. Both formats use `tau.agent_trace_compact`, schema version `0`, and contain
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

Use `tau-jsonl` only for complete replay or journal-integrity investigation. It
contains every selected canonical journal occurrence.

Tracing is a finite read-only snapshot. An inactive journal uses its locked EOF;
a currently loaded/running journal uses an exact validated checkpoint prefix, so
the trace may be stale and omit the newest writes. It never contacts the harness,
pauses the writer, or follows later writes.

Keep all trace artifacts private and use owner-only files. Compact and native
formats can contain unredacted reasoning, messages, arguments, outputs, images,
and secrets. Use session `events.jsonl` only for transient observations, private
provider captures only for exact wire shape, and component logs for operational
failures; load `tau-self-knowledge-debugging` before that escalation.

`docs/agent-trace.md` owns exact schemas and field definitions.
