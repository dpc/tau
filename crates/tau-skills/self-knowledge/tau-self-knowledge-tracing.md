---
name: tau-self-knowledge-tracing
description: Use when auditing Tau agent execution, performance, cost, session stats, chat export, orchestration, or durable semantic traces.
---

# Durable agent tracing

Choose the least sensitive projection:

```console
tau agent trace <agent-id> --include-descendants --format agent-performance-jsonl
tau agent trace <agent-id> --include-descendants --format agent-tools-toon
tau agent trace <agent-id> --include-descendants --format tau-jsonl
```

- `agent-performance-jsonl`: content-free usage/cost, tools/background calls,
  typed waits/effective timeouts, outer turns, and standalone compaction attempts.
  It still reveals identities, models, timing, membership, and work patterns.
- `agent-tools-toon` (CLI default, lite) or `agent-tools-jsonl`: semantic
  explanation, including unredacted prose/reasoning, messages, tool arguments
  and output. Lite caps each text/output item at 4 KiB but retains complete
  `text_bytes`/`text_lines` and `output_bytes`/`output_lines`; full keeps content.
- `tau-jsonl`: every selected canonical journal occurrence, for replay or
  journal-integrity investigation.

Compact formats use `tau.agent_trace_compact`, schema `0`, with `call`,
`activation`, and `relationship` items plus prose/messages. Items include relative
append time, optional Unix append time, and owning journal sequence. Only explicit
observation/call/message references establish correlation; cross-agent adjacency
and wall-clock order do not establish causality. Missing facts remain
`source_not_selected`, `unresolved`, or `incomplete`.

Qualified `*_us` intervals require selected, explicitly linked, nondecreasing
endpoints; there is no generic `duration_us`. Completion waits reference terminal
owners through `output_ref` rather than copying output.

Tracing takes a finite read-only snapshot: locked EOF for inactive journals,
validated checkpoint prefix for running ones. It does not contact the harness,
pause writers, or follow writes; checkpoints may omit recent activity.
Historical identities/timestamps do not change when journals move sessions.

## Summaries and exports

- `tau session stats --session SESSION_ID`: durable membership/journal summary;
  marks unavailable facts.
- `tau agent export chat AGENT_ID`: private durable-branch conversation with
  metadata, prompts, and responses. Markdown by default; `--toons` selects
  strict TOON.
- Live UI `$self/$subtree`: approximate equivalent-USD for an agent and its
  authenticated same-session creator descendants, including completed children,
  not messaged peers. It resets with the active session/runtime and may cover
  a different window than offline traces.

None is a provider invoice; coverage and fallback prices limit estimates.
Keep trace artifacts in owner-only files. Content-free metadata is still
sensitive; other formats may include images and secrets as well as text.

Use session `events.jsonl` for transient observations, private provider captures
for exact wire shape, and component logs for operational failures; load
`tau-self-knowledge-debugging` before escalating. `docs/agent-trace.md` owns
schemas and field definitions.
