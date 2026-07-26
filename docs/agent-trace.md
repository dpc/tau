# Durable agent trace export

`tau agent trace <agent-id>` projects from a validated snapshot of existing
durable agent journals offline. It defaults to a compact TOON-lite virtual-tool
overview; use explicit `--format tau-jsonl` for the complete journal artifact. It
does not contact or attach to a harness and does not capture transient provider
HTTP bodies, streaming deltas, or harness phase timing.

```console
tau agent trace <agent-id> \
  [--include-descendants] \
  [--format tau-jsonl|otlp-json|agent-tools-toon|agent-tools-jsonl] \
  [--mode lite|full] \
  [--agents-dir <path>]
```

The defaults select only the requested agent, `agent-tools-toon`, `lite`, and
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

Compact agent-tool projection also stages payload-bearing arguments and full
outputs in an anonymous file. Its heap correlation state contains call IDs,
timing/order keys, tool names, and file offsets. Lite computes output counts while
reading terminals and does not retain their bodies. Heap still grows with call
count plus encoded call-ID and tool-name bytes across the selected workflow, so
pathological high-cardinality journals can exhaust memory.

TOON materializes one call at a time. An exceptional near-limit argument can
simultaneously occupy its decoded value, amplified `TaggedCbor`, compact JSON
bytes, roughly 4/3-size Base64 text, and TOON scalar while the anonymous final
artifact also grows on disk. A pathological but frame-valid call can exhaust
memory or temporary storage. Such projection failure occurs before stdout; all
anonymous files remain delete-on-close.


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


## Compact agent tool traces

`agent-tools-toon` and `agent-tools-jsonl` emit the same stable compact projection
optimized for quick logical reconstruction of agent activity. They include only
virtual tool calls declared in durable provider responses. Extension-originated
tool requests, assistant prose, prompts, messages, compactions, and low-level
lifecycle events are omitted. `--mode` selects `lite` (the default) or `full`;
`--mode full` is invalid with `tau-jsonl` and `otlp-json`.

Each artifact starts with a `tau.agent_tools` schema-version-0 header. JSONL emits
the header and every call as independently parseable lines. TOON emits one strict
document whose header fields precede a counted `calls[N]` array. Calls appear by
projected journal wall-clock across included agents. This is not a causal or
authoritative cross-agent chronology. Equal timestamps order by agent ID, journal
sequence, then provider item position. `at_us` is measured from the earliest
included journal occurrence and `duration_us` from provider declaration to its
terminal fact. Neither encoding emits an absolute timestamp. A decreasing terminal
clock leaves `duration_us` absent.

The lite variant omits output bodies and reports `output_bytes` (UTF-8 bytes) and
`output_lines` (Rust `str::lines` logical lines). The full variant emits an
event-native normalized text projection as `output` (or exceptional
`output_base64`) and omits those counters:
successful CBOR uses `ToolResponse`, errors prepend an `error` header to normalized
details, and cancellation renders `cancelled: cancelled`.
Incomplete calls have status `incomplete`; their lite counts are zero and their
full output field is absent. Other statuses are `ok`, `error`, and `cancelled`.

Header fields are `schema: string`, `schema_version: integer`,
`record_type: "header"`, `root_agent_id: string`,
`included_agent_ids: string[]`, `output: "counts"|"full"`, and
`time_unit: "microseconds"`. Call fields are `record_type: "call"`,
`at_us: u64`, `agent_id: string`, call identity, `tool: string`,
optional command, arguments, `status`,
and optional `duration_us: u64`. Lite always adds `output_bytes: u64` and
`output_lines: u64`; full adds output only for completed calls.
Arguments use the concise ordinary JSON-shaped value only when every nested value
is represented faithfully and contains no float. JSONL otherwise places complete
`TaggedCbor` in `arguments`, including exact float bits. TOON retains the readable
call envelope and uses `call_id_base64` instead of `call_id` for a control-bearing
ID. It uses `arguments_json_base64` instead of `arguments` for Base64 compact JSON whenever
arguments contain tagged-CBOR or unsafe controls. Rare control-bearing commands
and outputs similarly use `command_base64` and `output_base64` as Base64 UTF-8.
Consumers decode only that exceptional field. This avoids ambiguous
nested-object-array framing, numeric normalization, raw terminal controls, and
Base64 expansion of the rest of the call while remaining lossless. Direct TOON quotes and escapes embedded
full-output LF, CR, tab, quote, and backslash characters; multiline output remains
one structurally framed scalar inside `calls[N]`. Schema version `0` follows the
same internal compatibility policy as native JSONL.

Shell tools named `shell`, `shell_command`, or the internal `gpt_shell` surface
normally expose their `command` argument as a top-level field for immediate
readability. The complete provider-declared argument value normally remains in
`arguments`, including the command; exceptional Base64 fields replace either one
as described above.

```console
$ tau agent trace agent-root --format agent-tools-toon
schema: tau.agent_tools
schema_version: 0
record_type: header
root_agent_id: agent-root
included_agent_ids[1]: agent-root
output: counts
time_unit: microseconds
calls[1]:
  - record_type: call
    at_us: 2419
    agent_id: agent-root
    call_id: call_1
    tool: shell_command
    command: cargo test -p tau-core
    arguments:
      command: cargo test -p tau-core
      workdir: /work
    status: ok
    duration_us: 180442
    output_bytes: 1372
    output_lines: 24
```

Use `--include-descendants` to obtain one relative journal-wall-clock projection
for a complete creator-owned workflow.

> **Sensitive data:** Both compact encodings expose unredacted tool names,
> arguments, and commands. `--mode full` additionally exposes complete
> unredacted output, including rendered error details. Treat either artifact as
> sensitive.
