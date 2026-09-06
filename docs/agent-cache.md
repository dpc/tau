# Offline cache evidence

`tau agent cache` and `tau session cache` inspect existing durable state without
contacting a daemon or provider. They write nothing unless `--index PATH` is
explicitly requested; source repair and capture enablement never occur.

```console
tau agent cache AGENT --include-descendants
tau agent cache AGENT --format jsonl --prompt PROMPT
tau agent cache AGENT --prompt PROMPT --view attribution
tau agent cache AGENT --view continuity
tau agent cache AGENT --view geometry --group-by model,backend,controls
tau agent cache AGENT --since 2026-09-06T00:00:00Z --model provider/model
tau agent cache AGENT --operation inference --attempt 2
tau agent cache AGENT --view geometry --require-exact-chain
tau agent cache AGENT --view geometry --format jsonl --index ./cache.private-index
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

The private capture reader recognizes current Chat Completions, public
Responses, and Codex request/response envelopes, Chat/Responses failures, and the
existing Codex finite-attempt and compact HTTP failure envelopes. It also recognizes
version-0 scalar cache captures as `diagnostic_files`. Current scalar records are
deduplicated by provider-instance/process record identity; conflicting reuse is
corruption rather than last-write-wins. Dispatch and attempt-end records join only by
their explicit capture-local attempt identity, never by adjacency or timestamps.
It streams compressed files
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

`--view attribution` projects the producer's explicit status, entries, raw usage,
and top-level reconciliation result. Current built-in adapters have no established
per-item wire table, so they report `unsupported_shape`; the inspector does not
invent entries from normalized accounting.

`--view continuity` reports actual captured dispatch counts, request form,
anchor-validation, connection and repair facts, and the typed attempt outcome.
Missing attempt-end records remain partial. Visible-prefix equality, route equality,
provider receipt, billing, and residency remain unknown unless separate exact
evidence establishes them.

`--view geometry` groups attempt-end reads only when a matching scalar dispatch has
the same observed backend, transport, model, reasoning, tool-choice, tier, and cache
controls. It reports sorted reads, observed maxima, and a GCD explicitly labeled
empirical. This is useful for regime/change tracking, not proof of token boundaries
or provider cache geometry. Non-summary views emit JSONL even when `--format` is
omitted.

When complete exact request captures have current attempt correlation,
`--view geometry` also compares complete captured JSON structure, closed controls,
tools, instructions, remaining request fields, route identity, cache-key equality,
ordered input-item prefixes, and explicitly captured response-chain edges. Values,
field names outside closed categories, provider IDs, cache keys, and fingerprints
are replaced by report-local ordinal labels. Object member order is canonicalized;
array order and JSON scalar type/value remain significant. These results describe
captured request structure, not the producer's original serialized whitespace,
provider tokenization, upstream receipt, eligibility, or cache residency. A
chained suffix without all required captured objects remains unavailable rather
than being reconstructed as a full prefix.

`--index PATH` replaces one disposable JSON index through an owner-private sibling
temporary file and rename, without fsync. The index retains the same random keyed
BLAKE3 key on later invocations at that exact path plus fixed-size structural
evidence; it contains no request bodies or raw provider/cache identifiers. It is
still private linkable workload data. Existing indexes must be regular,
owner-private, matching-build files and fit one quarter of the configured memory
budget. Invalid, shared, symlinked, oversized, or revision-skewed indexes fail
closed. Tau does not discover, retain, or delete these explicit exports; delete
them manually.

Shared `--since` and `--until` bounds require absolute RFC3339 timestamps and
are inclusive. `--model`, `--operation`, and `--attempt` select only directly
observed values; unavailable fields are excluded and counted rather than
inferred. The attempt selector uses the existing logical ordinal when present
and otherwise the explicitly supplied harness provider attempt. Geometry groups
by `model,backend,controls` by default; `--group-by` accepts any nonempty unique
subset of those dimensions. `--require-exact-chain` excludes scalar and exact
comparisons that lack a captured, matching response-chain edge. Exclusion counts
do not imply exhaustive capture history, and missing evidence remains a partial
result.

Exact
request/response capture remains **default-on
for durable activity**, independently of the metadata setting below. This command
does not alter capture, retention, inference, retry, refresh, or compaction policy.

## Private runtime metadata

ChatGPT/Codex, public Responses and Chat Completions ordinary inference and
standalone compaction produce bounded scalar captures in the
existing owner-private session `debug/provider-requests/<instance>/` directory,
using the `cache-diagnostic.json.zst` filename class. Set
`"cache_diagnostics": "off"` in a ChatGPT, public Responses, Chat Completions or OpenRouter provider profile to disable only these
metadata records, or `"metadata"` (the default) to enable them. Restart after
changing this startup-frozen setting. Ephemeral/memory-only activity remains
excluded under the existing capture permission; no filesystem probing infers
durability.

The capture payload uses `schema: "tau.cache_diagnostic"`, internal version `0`.
Its build identity comes from the executable's existing source metadata
(uninitialized library embeddings explicitly report `unknown`). Each process
has a random 128-bit `producer_run_id`; each finite inference or compaction attempt has a
separate random `attempt_id`. Exact inference request/response/failure captures
carry that attempt identity and the actual dispatch index where one exists.
An unsent exact request retained after cancellation has a null wire index.
Public Responses exact requests always retain null at their unchanged capture
point before the final cancellation check; only their diagnostic dispatch row
establishes actual index one. Later exact responses/failures can carry one.
These identities never affect provider routing or upstream request bodies.
Native compaction uses `operation: "standalone_compaction"` and its existing
finite compact-attempt ordinal. Its exact requests and retry-failure captures
share that identity; successful raw compact responses remain unretained, so
`exact_response` is false for this operation. Scalar usage comes from the parsed
terminal event before it is dropped. The summary follows the final compact
outcome after output validation and cancellation, including zero-dispatch
admission failures; it does not authorize a retry after semantic compact output.

`dispatch` records observe attempted enqueue of the final serialized envelope,
including failed enqueue to a closed writer—not transport acceptance, successful
send, provider receipt or billing. `attempt_end` records report the
dispatch count and success/error/cancellation/pre-dispatch outcome. Repair used
and another dispatch are independent: a failed replacement upgrade can spend
repair while leaving the dispatch count at one. Typed socket epochs, reuse,
anchor-validation and repair facts describe only branches the backend observed.
No endpoint, credential, cache-key literal, provider response ID, previous-response
ID, error prose or raw payload enters scalar records.
Public Responses uses a fresh finite full-replay invocation with at most one
dispatch and no local repair or connection reuse. Its logical-attempt ordinal is
unavailable; its harness provider attempt comes directly from the extension.
Local-summary records use `operation: "standalone_compaction"` and the existing
prompt identity. Their attempt end reports the backend result before the
built-in extension validates the summary narrative, so backend success can
coexist with canonical compaction rejection.

Raw provider input/read/write/output counters remain separate from normalized
canonical accounting. Missing counters, exact eligibility, model revision and
unavailable chain counts stay null. There is no established per-item attribution
parser yet: `raw_attribution` is false, the attribution array stays empty, and
present usage is labeled `unsupported_shape` for attribution.
Chat Completions retains the latest observed allowlisted usage member, even on
later failure, without merging or normalizing repeated members. Its raw cache
counters follow the exact route's selected OpenAI or DeepSeek schema; unselected
schemas do not become evidence. Its exact captures share the actual dispatch's
attempt identity and index. Local-summary compaction preserves the existing
scheduler ordinal in both logical and harness-attempt fields; its attempt end
likewise precedes the built-in extension's final narrative validation.
Entered Codex prewarm/cache-refresh backend calls produce scalar-only
`operation: "cache_refresh"` records. This label does not prove the automatic
refresh scheduler selected the work. Explicit refresh keeps its existing
`operation_id`; ordinary prewarm gets a random operation-local identity.
`agent_prompt_id`, `logical_attempt` and `harness_provider_attempt` are null.
Both exact-capture capabilities are false: no warm request or response body is
newly retained. The separate managed filename is
`<micros>.cache-operation.<attempt-id>.cache-diagnostic.json.zst`, not a synthetic
prompt filename.

Warm `attempt_end` describes the backend result after socket publication: installed
is success, busy is a zero-dispatch pre-dispatch failure, and cancellation/error
retains its backend meaning. A later worker deadline override or rejected/stale
harness terminal does not rewrite it. Profile resolution, cooldown and supervisor
rejections before backend entry remain outside coverage. Parsed raw usage is
retained only while available; discarded socket-publication results can leave
usage unknown. Nothing here changes refresh eligibility, preemption, scheduling,
repair, accounting or cache-residency claims. The inspector recognizes operation
captures for backend continuity without inventing a prompt join. Harness owner
selection, deadline override, terminal acceptance, and accounting remain unavailable.

Records are capped at 256 KiB inclusive, with each allowlisted identity capped
at 128 UTF-8 bytes; over-limit identities are omitted whole with fixed flags.
Metadata reserves a complete record before optional evidence construction, with
at most 64 reservations / 16 MiB including in-flight serialized data. It shares
the existing nonblocking lossy capture worker and opaque protocol transport;
raw-capture budgets do not change. Sequence exhaustion disables new records.
Sequence holes and later cumulative known-loss counts expose some losses, not
exhaustiveness. No wait, retry, drain, shutdown join or fsync is added. An
already-started capture frame can still delay a following terminal on ordinary
non-preemptive IPC.

The new class inherits owner-only storage, crash/torn-write behavior and managed
diagnostic retention (thirty days by default, configurable/disableable). It adds
no journal fields, canonical accounting, provider probes, cache refreshes,
automatic index or cache-eligibility authority.
