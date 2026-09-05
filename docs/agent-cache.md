# Offline cache evidence

`tau agent cache` and `tau session cache` inspect existing durable state without
contacting a daemon or provider. Nothing is written: no index, source repair,
temporary staging artifact, or capture enablement.

```console
tau agent cache AGENT --include-descendants
tau agent cache AGENT --format jsonl --prompt PROMPT
tau session cache SESSION --format jsonl
```

This is the first, deliberately limited delivery of cache diagnostics. Summary
reports canonical response counts and evidence gaps. JSONL exposes response-local
normalized usage, independently recorded eligible ceilings, historical cost
rates/increments, backend/transport, chain-fallback and pool facts. It does not
print upstream response IDs, endpoints, bodies, arbitrary metadata, or source
paths. Keep the output private: local agent/prompt IDs, models, timing, and
workload accounting are content-free, **not public-safe**.

Only accepted `ProviderResponseFinished` journal occurrences count as canonical
responses. Incoming reports and debug JSONL do not count. Agent selection uses
authenticated creator edges; session selection uses durable membership and then
filters prompt session attribution. Each selected agent is validated through the
existing strict finite-prefix snapshot reader. JSONL records the selected
per-agent sequence boundary; those boundaries are not cross-agent causal order.

The private capture inventory recognizes current Chat Completions, public
Responses, and Codex request/response envelopes, Chat/Responses failures, and the
existing Codex finite-attempt and compact HTTP failure envelopes. It streams compressed files
one at a time and retains only typed session/prompt attribution and file counts.
These are **file counts, not attempt or dispatch counts**. Identical legacy files
still count as two files; there is no stable record identity to deduplicate.
Multiple same-prompt files expose ambiguous terminal association. Even one
request and response remain `legacy_partial`: neither adjacency nor timestamps
establish an exact join, and best-effort capture history is never exhaustive.

JSONL uses `tau.cache_diagnostic`, internal schema version `0`, and the executable
build identity. `canonical`, `reported`, and `derived` remain separate.
`reported` is null in this delivery: normalized capture usage is not silently
relabelled raw provider evidence. Every arithmetic result identifies its
canonical source. Reads divided by input are a share of input; input minus reads
is non-read input, **not cache misses**. Unknown eligibility/residency stays
unknown. Invalid counters are not capped into valid evidence. Recorded costs
remain per-response facts; failed-attempt billing is unknown.

Exit status:

- `0`: requested analysis completed without encountered required-source gaps.
- `2`: invalid invocation or unsupported canonical/capture encoding. Semantic
  journal decode errors cannot distinguish revision skew from malformed typed
  fields and fail here rather than selecting a compatibility fallback.
- `3`: useful partial report, including unavailable capture evidence, malformed
  or truncated files, ambiguous association, and resource admission limits.
  Missing or structurally corrupt canonical sources report unavailability without
  claiming inspection.

The default capture limits are 16 MiB compressed/file, 64 MiB decoded/file, 1 GiB
cumulative decoded, and a 512 MiB working-memory budget. Override these with
`--max-compressed-bytes`, `--max-decompressed-bytes`, `--max-total-bytes`, and
`--max-memory-bytes`. All are byte counts. The parser uses a conservative
allocation charge, so its effective decoded-file admission can be lower than the
nominal file limit. Compression windows are capped at 8 MiB.

Stage 1 retains strict canonical replay unchanged. Before admitting it, the
inspector charges 128 times the cumulative selected journal bytes (including
session membership) against one quarter of the memory budget. Over-limit
preflight produces explicit partial/unavailable output and **does not inspect
the snapshot**. This is a conservative byte-charge proxy, not a measured peak-RSS
guarantee; it can reject a small live checkpoint backed by a large journal.
Capture parsing/inventory and in-memory output have separate bounded shares.
Explicit capture loss and output truncation remain partial results. The source
directories are never changed.

Attribution, continuity, geometry, shared filters beyond `--prompt`, disposable
indexes, new capture-local correlation, and runtime metadata are subsequent
deliveries. Their CLI surfaces are absent rather than empty-success placeholders.
Exact payload capture remains separately opt-in/default-off; this command
does not alter capture, retention, inference, retry, refresh, or compaction policy.
