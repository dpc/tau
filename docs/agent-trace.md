# Durable agent trace export

`tau agent trace <agent-id>` exports existing durable agent journals offline. It
does not contact or attach to a harness and does not capture transient provider
HTTP bodies, streaming deltas, or harness phase timing.

```console
tau agent trace <agent-id> \
  [--include-descendants] \
  [--format tau-jsonl|otlp-json] \
  [--agents-dir <path>]
```

The defaults select only the requested agent, `tau-jsonl`, and
`<state-dir>/agents`. Machine output goes to stdout; diagnostics go to stderr.
A closed stdout pipe is a successful early consumer exit.


## Snapshot and failure behavior

Tau opens only existing lock and journal files. It acquires all selected locks
nonblocking in lexical agent-ID order, rechecks creator-based descendant
discovery under those locks, and strictly validates every complete journal.
Missing, ephemeral-only, active, corrupt, torn, or concurrently changed
workflows fail without stdout output. `parent_agent`, session membership, and
message peers do not imply descendant membership.

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


## Native JSON Lines

`tau-jsonl` is the complete canonical artifact. Its schema identifier is
`tau.agent_trace` and its internal schema version is `0`. The first line is a
header; later lines preserve every journal occurrence grouped lexically by
agent and ordered by authoritative `seq`. Each occurrence retains agent ID,
sequence, wall-clock append time, source, branch parent, and complete typed
event payload.

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
[`DECISION-no-backward-compatibility`](../specs/DECISION-no-backward-compatibility.md).


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
