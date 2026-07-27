# Durable agent trace export

`tau agent trace <agent-id>` projects offline from a validated finite snapshot
of existing durable agent journals. It defaults to a compact TOON-lite
agent-tool overview; use explicit `--format tau-jsonl` for the complete journal
artifact. It does not contact or attach to a harness and does not capture transient provider
HTTP bodies, streaming deltas, or harness phase timing.

```console
tau agent trace <agent-id> \
  [--include-descendants] \
  [--format tau-jsonl|otlp-json|agent-tools-toon|agent-tools-jsonl|agent-performance-jsonl] \
  [--mode lite|full] \
  [--agents-dir <path>]
```

The defaults select only the requested agent, `agent-tools-toon`, `lite`, and
`<state-dir>/agents`. Machine output goes to stdout; diagnostics go to stderr.
A closed stdout pipe is a successful early consumer exit.
`--mode full` is valid only with `agent-tools-toon` and
`agent-tools-jsonl`; every other format is content-fixed.


## Snapshot and failure behavior

Tau opens only existing lock, journal, and checkpoint files. It first attempts
each selected lock nonblocking in lexical agent-ID order. For an inactive
journal it acquires the exclusive lock before selecting the current EOF. When a
writer still holds the lock, Tau uses the existing atomic, journal-bound
checkpoint to select the last published committed prefix. It retains each exact
opened journal identity and strictly validates every selected prefix before
output.

The command is a one-shot snapshot: it reads through one finite committed cut
and never waits for writer exit or includes records appended after that cut. No
follow mode is implemented. Missing, ephemeral-only, unsupported, corrupt,
torn, or concurrently replaced selected prefixes fail without stdout output,
as does a lock-held journal without a usable committed checkpoint.
`parent_agent`, session membership, and message peers do not imply descendant
membership.

Discovery treats only a valid sequence-zero `AgentStarted.creator` fact as an
authenticated edge. Unreadable or unsupported artifacts that cannot establish
an edge remain outside the rooted workflow and cannot abort its trace. Once an
edge makes a journal reachable, its selected prefix undergoes strict validation;
unsupported or corrupt content inside that prefix fails the trace explicitly.

Tau stages the finished artifact in a private process-owned temporary file
before stdout delivery. The anonymous file has no pathname to survive process
termination, is never durable trace state, and cannot be selected as an output
destination. Validation and projection stream journal records. OTLP stages every
correlated occurrence in an anonymous file, including auxiliary occurrences
whose offsets are not retained; heap correlation state
contains compact offsets and identifiers, one per unique typed operation key.
Heap use is therefore proportional to the number and encoded length of unique
operation IDs in the largest included journal; IDs are not independently capped
beyond the journal record framing limit, so a pathological journal can exhaust
process memory. No accepted record is truncated.

Compact agent-tool projection materializes the selected event payloads and its
complete JSON-like record set in memory before encoding. Lite mode bounds each
projected terminal output to 4 KiB but does not bound declaration arguments or the
selected source events retained during correlation. Full mode additionally retains
complete rendered terminal output. Heap use can therefore grow with both selected
journal payload bytes and projected record bytes; a pathological frame-valid
journal can exhaust memory or anonymous temporary storage. Projection still
finishes before stdout delivery, and the final staged artifact remains
delete-on-close.


## Native JSON Lines

`tau-jsonl` is the complete canonical artifact. Its schema identifier is
`tau.agent_trace` and its internal schema version is `0`. The first line is a
header; later lines preserve every journal occurrence grouped lexically by
agent and ordered by authoritative `seq`. Each occurrence retains agent ID,
sequence, observation ID, wall-clock append time, source, branch parent, and
complete typed event payload.

The header fields are:

```text
schema:                    string, always "tau.agent_trace"
schema_version:            integer, always 0
record_type:               string, always "header"
root_agent_id:             string
included_agent_ids:        string[], lexical order
timing:                    string, always "journal_wall_clock"
```

Every following event line has:

```text
schema:                    string, always "tau.agent_trace"
schema_version:            integer, always 0
record_type:               string, always "event"
agent_id:                  string
seq:                       integer, journal-local authoritative order
observation_id:            string, 32 lowercase hexadecimal digits
recorded_at_unix_micros:   integer
source:                    string|null
parent:                    AgentEventParent JSON
event:                     {event: string, payload: TaggedCbor}
```

`AgentEventParent` has these exact forms:

```text
{"kind":"inherit_head"}
{"kind":"root"}
{"kind":"under","node_id":u64}
```

Event payloads use tagged-CBOR JSON. Integers are decimal strings, floats are
exact IEEE-754 bit strings, bytes are base64, maps are ordered key/value entry
arrays, and arrays and tags remain explicit. This representation preserves
CBOR bytes, non-string map keys, integer extremes, tags, and non-finite floats
without JSON coercion.

`TaggedCbor` has these exact discriminated shapes:

```text
{"type":"null"}
{"type":"bool","value":bool}
{"type":"integer","value":decimal-string}
{"type":"float64_bits","value":16-lowercase-hex-digits}
{"type":"text","value":string}
{"type":"bytes","encoding":"base64","value":string}
{"type":"array","value":TaggedCbor[]}
{"type":"map","value":[{"key":TaggedCbor,"value":TaggedCbor}, ...]}
{"type":"tag","tag":decimal-string,"value":TaggedCbor}
```

Schema version `0` is an internal-format contract. Tau provides no compatibility
or migration reader for later incompatible revisions, following
[`GATE-no-backward-compatibility`](../specs/GATE-no-backward-compatibility.md).


## OTLP JSON

`otlp-json` emits one protobuf-JSON `ExportTraceServiceRequest` for Phoenix,
Langfuse, and other OTLP/OpenInference consumers. It is a lossy visualization
adapter, not a replacement for `tau-jsonl`.

Tau derives agent, outer-turn, prompt/provider, tool, message, and compaction
spans only from their typed durable IDs. Explicit start and terminal facts set
boundaries. Unmatched operations or any decreasing journal timestamp become
instant spans marked `tau.incomplete`; timestamps never order different
agents. Every durable occurrence is also attached verbatim in native lossless
form as an event on its agent span. OpenInference attributes carry available
input/output, model parameters, tool arguments/results, usage/cache/cost facts,
Tau IDs, branch parent, and journal sequences. For a durable provider response
containing multiple tool calls, each TOOL span receives a compact projection of
only its matching call item; response-wide payloads are not repeated per call.


## Compact agent tool traces

`agent-tools-toon` and `agent-tools-jsonl` project only provider-declared calls and explicit observation facts. The in-place `tau.agent_tools` schema remains version `0`; no legacy reader or inference path exists. References outside the selected cut remain unresolved. Duplicate observation IDs and contradictory selected typed references are trace integrity errors.

The header fixes `timing_basis: producer_wall_clock_at_observation` and `causality: explicit_observation_refs_only`. Records use `call`, content-free `activation`, and content-free `relationship` discriminators. `ToolCallRef` (`declaration`, `item_index`) is the call identity; provider `call_id` remains display and routing metadata. Journal order and timestamps never create relationships.

Only explicitly linked, nondecreasing endpoint pairs emit qualified intervals: `declaration_to_dispatch_us`, `dispatch_to_backgrounded_us`, `backgrounded_to_terminal_us`, `dispatch_to_terminal_us`, `active_wait_us`, `completion_to_delivery_us`, `activation_to_wait_terminal_us`, and `completion_to_activation_queue_us`. There is no unqualified `duration_us`.

Wait relationships report registration as `immediate`, `active`, or `unresolved`, and outcome as `completion_delivered`, `interrupted_by_activation`, `input_available`, `timed_out`, `rejected`, `cancelled`, `lifecycle_aborted`, or `incomplete`. Missing referenced observations remain explicit as `source_not_selected`, `unresolved`, or `incomplete`; the projector never reconstructs them from adjacency, timestamps, IDs, or prose.

A canonical terminal owns normalized output exactly once. Lite mode emits its exact `output_bytes` and `output_lines` plus bounded output; full mode emits complete `output` (or TOON `output_base64` where required). A completion-delivering wait emits only `output_ref` and `envelope`, never copied payload or counts. JSONL and TOON preserve the same identities, lifecycle, wait outcomes, relationships, and ownership; only payload representation differs.

JSONL writes one header followed by independently parseable records. TOON writes
the same header fields, then a strict `records[N]:` counted array in the same
record order. Optional fields below are absent unless their stated evidence
survives:

TOON preserves strings directly when its grammar can round-trip them. Otherwise
it replaces the whole field with standard padded Base64: `call_id_base64`,
`command_base64`, or `output_base64` decodes to the UTF-8 bytes of the
corresponding JSONL string. `arguments_json_base64` decodes to the complete
compact JSON encoding of `arguments`; consumers must Base64-decode and then parse
that whole JSON value. A record never emits both the direct and Base64 spelling
of one field. Base64 is a framing adaptation only and does not change semantic
record parity with JSONL.

```text
header:
  schema="tau.agent_tools", schema_version=0, record_type="header",
  root_agent_id, included_agent_ids[], output="lite"|"full",
  time_unit="microseconds",
  timing_basis="producer_wall_clock_at_observation",
  causality="explicit_observation_refs_only"

call:
  record_type="call", agent_id, call={declaration,item_index},
  call_id, tool, arguments, status
  [command] [terminal] [terminal_resolution] [cause]
  [output,output_complete] [output_bytes,output_lines]
  [qualified *_us intervals]

activation:
  record_type="activation", agent_id, observation_id, kind,
  source_observation|null, source_call|null, source_resolution|null,
  [completion_to_activation_queue_us]

relationship/wait_registration:
  record_type="relationship", relationship="wait_registration",
  agent_id, observation_id, wait_observation, wait_call, mode, registration="active",
  outcome="settled"|"incomplete"

relationship/wait_observation:
  record_type="relationship", relationship="wait_observation",
  agent_id, observation_id, wait_call, mode

relationship/wait_settlement:
  record_type="relationship", relationship="wait_settlement",
  agent_id, observation_id, wait_call, registration, registration_ref|null,
  wait_observation,
  wait_terminal, wait_terminal_resolution, outcome,
  [source_call,source_phase,output_ref,envelope|activation_ref],
  [source_resolution], [reason],
  [active_wait_us|completion_to_delivery_us|activation_to_wait_terminal_us]

relationship/cancellation_requested:
  record_type="relationship", relationship="cancellation_requested",
  agent_id, observation_id, cancel_call, target_call
```

Resolution fields use `resolved` when the selected endpoint belongs to the same
agent journal and `source_not_selected` when it is unavailable to that
journal-local relationship. The latter includes selected endpoints in another
agent journal. Such relationships never produce elapsed intervals or transfer
terminal status/output ownership. Exact-wait targets outside the relationship's
journal project as `exact_unresolved`.

`ToolCallRef` always serializes as
`{"declaration":"<32-lowercase-hex>","item_index":<u32>}`. Observation references
use the same 32-digit encoding. Ordinary JSON-compatible arguments stay ordinary
JSON; values that JSON cannot preserve use the native tagged-CBOR shapes documented
above.

For example, a completion-delivering wait links rather than duplicates output:

```json
{"record_type":"call","agent_id":"agent-a","call":{"declaration":"11111111111111111111111111111111","item_index":0},"call_id":"source","tool":"shell","arguments":{},"status":"ok","terminal":"22222222222222222222222222222222","terminal_resolution":"resolved","cause":{"kind":"completed"},"output":"done","output_complete":true}
{"record_type":"relationship","relationship":"wait_settlement","agent_id":"agent-a","observation_id":"55555555555555555555555555555555","wait_observation":"77777777777777777777777777777777","wait_call":{"declaration":"33333333333333333333333333333333","item_index":0},"registration":"active","registration_ref":"44444444444444444444444444444444","wait_terminal":"66666666666666666666666666666666","wait_terminal_resolution":"resolved","outcome":"completion_delivered","source_call":{"declaration":"11111111111111111111111111111111","item_index":0},"source_phase":"background","output_ref":"22222222222222222222222222222222","envelope":"original_tool_call_id_header","source_resolution":"resolved"}
```

Use `--include-descendants` for the selected creator-owned workflow. Both formats expose unredacted tool names, arguments, commands, and owner output, so treat artifacts as sensitive.


## Content-free performance JSON Lines

`agent-performance-jsonl` projects only prompt correlations, response-local
token/cache accounting, stored estimated cost, and qualified lifecycle timing.
It never emits prompt or response content, errors, tool names, arguments,
results, model parameters, or provider bodies. Agent, prompt, and
provider-qualified model IDs, descendant membership, activity timing, token
counts, cache reuse, and cost remain sensitive metadata.

The first row uses schema `tau.agent_performance`, version `0`, and contains the
root/included agent IDs, `time_unit: "microseconds"`,
`timing_fidelity: "recorded_at_wall_clock_append_invocation_interval"`, and
`content_included: false`. Each included agent then emits provider-prompt rows
ordered lexically by prompt ID followed by one `agent_summary`; agents remain in
lexical order. `--include-descendants` uses the same authenticated creator scope
and snapshot as every other format.

```text
header: schema, schema_version, record_type, root_agent_id,
        included_agent_ids, time_unit, timing_fidelity, content_included
provider_prompt: record_type, agent_id, agent_prompt_id, model,
                 optional at_us, optional terminal_at_us,
                 optional recorded_at_wall_elapsed_us, terminal_present,
                 optional prompt_sent_tokens,
                 optional prompt_cached_tokens,
                 optional response_received_tokens,
                 optional estimated_api_cost_picodollars
agent_summary: record_type, agent_id, provider_prompt_occurrences,
               provider_prompt_complete, provider_prompt_incomplete,
               provider_prompt_elapsed_reported,
               provider_prompt_recorded_at_wall_elapsed_sum_us,
               optional prompt_sent_tokens,
               optional prompt_cached_tokens,
               optional response_received_tokens,
               optional cache_hit_ratio_ppm,
               optional estimated_api_cost_picodollars,
               usage_reported_occurrences, usage_missing_occurrences,
               cost_reported_occurrences, cost_missing_occurrences
```

A provider-prompt row contains `agent_id`, `agent_prompt_id`, the stable
provider-qualified `model`, terminal presence,
optional response-local sent/cached/output tokens, and optional exact stored
estimated-cost picodollars. Missing canonical journal values remain absent.
Genuine present zero values remain numeric zero. Cached tokens are capped at
sent tokens, matching Tau accounting. Summaries separately count terminal
occurrences with and without usage/cost evidence, sum only present evidence,
and emit `cache_hit_ratio_ppm = floor(1_000_000 * cached / sent)` only for a
nonzero present input-token total. Checked aggregate overflow fails projection
before stdout instead of emitting a saturated total. A stored per-response cost
may itself be the saturated estimate produced by Tau's cost type.

Ordinary-inference prompt materialization (`agent.prompt_started`) is the sole
start and inclusion evidence. Standalone compaction is excluded because its
canonical terminal is not `provider.response_finished`. The optional
`recorded_at_wall_elapsed_us` and its summary sum compare the journal records'
wall-clock append-invocation timestamps. They are not durable commit time,
provider wire/model latency, or exact execution time, and intervals can overlap.
Zero timestamps are unavailable; decreasing clocks omit the interval. Relative
offsets are absent when no nonzero trace origin exists. Missing terminals stay
explicitly incomplete. Duplicate lifecycle/terminal facts for one agent/prompt
correlation fail projection rather than selecting one.

Projection retains one small content-free correlation entry per distinct prompt
until that agent's rows are emitted. It does not retain provider output,
error text, model parameters, or cumulative token snapshots. As with other trace
formats, pathological prompt-ID cardinality or length can exhaust memory.
