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
or provider. They do not enable captures or repair sources; only explicit
`--index PATH` writes a disposable owner-private index. Agent descendants
follow authenticated creator edges; session selection follows durable
membership and prompt attribution. JSONL separates canonical, reported, and
derived evidence. Only accepted canonical provider-response journal records
count as responses; incoming reports and debug captures do not.

The default summary reports response counts and evidence gaps. `attribution`
shows explicit producer attribution, **not** inferred per-item wire costs;
current built-in adapters may report `unsupported_shape`. `continuity` shows
captured dispatch/connection/repair facts, not proof of upstream receipt or
cache residency. `geometry` groups compatible observed cache reads and can
compare captured request structures; its empirical quantities do not prove
provider token boundaries. Non-summary views emit JSONL by default.
Non-read input (`input - reads`) is **not** a count of cache misses. Neither
observed read share nor captured request similarity proves billing or upstream
cache eligibility/residency.

Exit `0` means the requested analysis completed; `2` means invalid invocation
or unsupported encoding; `3` indicates a useful partial report or unavailable
sources. Capture files are best-effort and can be missing, truncated, ambiguous,
or over resource limits. A live checkpoint may be stale. A missing capture
never proves a provider call did not happen; canonical source failure is not
silently replaced by a debug report. Resource limits and exact JSONL schema
are in `docs/agent-cache.md`.

Keep reports and `--index` output private: identifiers, timing, model and
workload data are content-free but not public-safe. Use
`tau-self-knowledge-debugging` for specific wire-shape questions that require
private provider captures or transient session events.
