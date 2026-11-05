---
name: tau-self-knowledge-cache
description: Use for Tau's offline agent/session cache inspector, cache usage and efficiency evidence, attribution, continuity, geometry, and partial-data diagnosis.
advertise: false
---

# Inspect offline cache evidence

Start with durable state, not a best-effort debug event log:

```console
tau agent cache AGENT --include-descendants
tau session cache SESSION
tau agent cache AGENT --prompt PROMPT --view attribution
tau agent cache AGENT --view continuity
tau agent cache AGENT --view geometry --group-by model,backend,controls
tau agent cache AGENT --format jsonl --since 2026-09-06T00:00:00Z --model provider/model
```

These commands take a finite read-only snapshot without contacting the daemon
or provider, enabling captures, or repairing sources. Only `--index PATH` writes
a disposable private index. Descendants follow authenticated creator edges;
sessions follow durable membership/prompt attribution. JSONL separates canonical,
reported, and derived evidence. Only accepted canonical provider-response journal
records count as responses, not incoming reports or captures.

- Default summary: response counts and evidence gaps.
- `attribution`: explicit producer attribution, not inferred per-item wire costs.
  Built-in adapters may report `unsupported_shape`.
- `continuity`: captured dispatch/connection/repair facts, not upstream receipt
  or cache residency.
- `geometry`: compatible observed reads and captured request structures, not
  proof of provider token boundaries.

Non-summary views default to JSONL. Non-read input (`input - reads`) is **not**
a cache-miss count. Read share/request similarity proves neither billing nor
upstream cache eligibility/residency.

Exit codes: `0` completed, `2` invalid invocation/unsupported encoding, `3`
partial report/unavailable sources. Captures may be missing, truncated, ambiguous,
or over limits; live checkpoints may be stale. Missing captures do not disprove
calls, and debug reports cannot replace failed canonical sources.
See `docs/agent-cache.md` for limits and schema.

Keep reports and `--index` output private: identifiers, timing, model and
workload data are content-free but not public-safe. Use
`tau-self-knowledge-debugging` for specific wire-shape questions that require
private provider captures or transient session events.
