---
name: tau-agent-tracing
description: >
  Use this skill when tracing or auditing Tau agent execution, including provider
  and cache cost, tool/background/wait latency, outer turns, compaction, delegated
  workflows, or performance regressions from durable agent journals.
user-invocable: true
advertise: true
---

# Trace Tau agent execution

Start content-free and include the workflow when relevant:

```sh
trace=$(mktemp)
chmod 600 "$trace"
tau agent trace <agent-id> --include-descendants \
  --format agent-performance-jsonl >"$trace"
jq -c 'select(.record_type != "header")' "$trace"
```

This view covers ordinary provider accounting, tool/background lifecycle, typed
waits, outer turns, and standalone compaction without prompt, tool, or response
bodies. Useful filters include:

```sh
jq -c 'select(.record_type=="wait" and .outcome=="interrupted_by_activation")' "$trace"
jq -s 'map(select(.record_type=="provider_prompt")) | sort_by(.recorded_at_wall_elapsed_us // 0) | reverse | .[:10]' "$trace"
```

Use `agent-tools-toon` or `agent-tools-jsonl` only to explain an identified
interval. Prefer the default lite/bounded output; use `--mode full` only when the
bounded payload cannot answer the question. Use `tau-jsonl` only for
journal-integrity or replay questions: it is the complete canonical artifact,
not the default performance view.

Escalate outside trace only for facts trace intentionally does not own: session
`events.jsonl` for transient runtime observations, private provider captures for
exact wire/request shape, and component logs for operational failures. Load
`tau-self-knowledge-debugging` for that workflow.

Interpret intervals narrowly. Journal sequence is authoritative only within one
agent. Cross-agent timestamp order is presentation, not causality; only explicit
references justify relationships. `*_us` measures append-invocation wall time,
not CPU, wire, model, or commit time, and totals can overlap. A running-agent
trace uses a validated checkpoint prefix and may omit the newest writes.

Performance JSONL contains no prompt/tool body, but it still exposes IDs, model,
activity timing, token/cache/cost, membership, and work patterns. Compact and
`tau-jsonl` outputs additionally expose unredacted reasoning, messages,
arguments, output, and possibly secrets. Keep traces private, use owner-only
files, do not put them in `/tmp/public`, reports, tickets, or commits, and delete
temporary artifacts.

`docs/agent-trace.md` owns exact schemas. Built-in
`tau-self-knowledge-tracing` covers installed Tau usage, and
`tau-self-knowledge-debugging` owns lower-level logs and captures.
