# Durable agent trace export

`tau agent trace <agent-id>` projects offline from a validated finite snapshot
of existing durable agent journals. It defaults to a compact TOON-lite
semantic timeline; use explicit `--format tau-jsonl` for the complete journal
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
journal it acquires a shared lock before selecting the current EOF. When a
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

Compact semantic projection materializes the selected event payloads and its
complete JSON-like item set in memory before encoding. Lite mode bounds each
projected semantic text and terminal output to 4 KiB but does not bound
declaration arguments or selected source events retained during correlation.
Full mode additionally retains complete semantic text and rendered terminal
output. Heap use can therefore grow with both selected journal payload bytes and
projected item bytes; a pathological frame-valid journal can exhaust memory or
anonymous temporary storage. Projection still finishes before stdout delivery,
and the final staged artifact remains delete-on-close.


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


## Compact semantic traces

`agent-tools-toon` and `agent-tools-jsonl` project provider-declared calls,
explicit observation relationships, assistant prose, displayable provider
reasoning, and explicit sent/received agent messages. The
`tau.agent_trace_compact` schema is version `0`; no legacy reader or inference
path exists. References outside the selected cut remain unresolved. Duplicate
observation IDs and contradictory selected typed references are trace integrity
errors.

The header fixes
`absolute_time: unix_epoch_microseconds_at_journal_append_invocation`,
`timing_basis: producer_wall_clock_at_observation`, and
`causality: explicit_observation_refs_only`. Items use `call`,
`assistant_message`, `assistant_reasoning`, `message_sent`, `message_received`,
content-free `activation`, and content-free `relationship` discriminators.
`ToolCallRef` (`declaration`, `item_index`) is the call identity; provider
`call_id` remains display and routing metadata.

Every item carries trace-relative `at_us`, optional
`recorded_at_unix_micros`, owning `agent_id`, and authoritative owning-journal
`journal_seq`. Provider response items also carry their zero-based
`item_index`. The relative origin is the minimum canonical `recorded_at` among
all occurrences in the selected snapshot, including occurrences omitted from
this compact projection. A zero timestamp omits absolute time and otherwise
retains the existing zero-origin behavior.

`journal_seq` is authoritative order within one agent journal. The combined
view sorts journal append-invocation wall-clock samples for readability and
uses deterministic ties. Across agents, wall-clock order, adjacency, and
equal/shared timestamps do not establish causality or delivery order. Shared
`message_id` correlates directional message facts but does not turn their
timestamps into a latency or happens-before measurement.

When every stronger sort component is equal, the approved family order is
`call`, `assistant_message`, `assistant_reasoning`, `message_sent`,
`message_received`, `activation`, then `relationship`.

Only explicitly linked, nondecreasing endpoint pairs emit qualified intervals: `declaration_to_dispatch_us`, `dispatch_to_backgrounded_us`, `backgrounded_to_terminal_us`, `dispatch_to_terminal_us`, `active_wait_us`, `completion_to_delivery_us`, `activation_to_wait_terminal_us`, and `completion_to_activation_queue_us`. There is no unqualified `duration_us`.

Wait relationships report registration as `immediate`, `active`, or `unresolved`, and outcome as `completion_delivered`, `interrupted_by_activation`, `input_available`, `timed_out`, `rejected`, `cancelled`, `lifecycle_aborted`, or `incomplete`. A successful plural exact wait emits one `completion_delivered` relationship per source in request order; those records share the wait call and terminal and copy no source payload. Missing referenced observations remain explicit as `source_not_selected`, `unresolved`, or `incomplete`; the projector never reconstructs them from adjacency, timestamps, IDs, or prose.

A canonical terminal owns normalized output exactly once. Lite mode emits its exact `output_bytes` and `output_lines` plus bounded output; full mode emits complete `output` (or TOON `output_base64` where required). A completion-delivering wait emits only `output_ref` and `envelope`, never copied payload or counts. JSONL and TOON preserve the same identities, lifecycle, wait outcomes, relationships, and ownership; only payload representation differs.

Resolved calls declared as `shell`, `shell_command`, or `gpt_shell` include an
optional `shell_outcome` only when their selected canonical terminal contains a
coherent raw structured result. This object is identical in lite/full and
JSONL/TOON:

```json
{
  "source": "tool_result",
  "success": false,
  "termination_reason": "exit",
  "exit_code": 100
}
```

`source` is `tool_result` or `tool_error_details`; `termination_reason` is
`exit`, `timeout`, `signal`, `start_error`, or explicitly recorded `unknown`.
`exit_code` and `signal` are exact signed 32-bit integers when available, and
`timed_out` appears only as `true`. `success` means a coherent normal exit with
code zero. In particular, top-level `status: ok` means the virtual call reached
a completed terminal and can coexist with `shell_outcome.success: false` for a
nonzero exit, timeout, or signal. To select known failed shell processes use
`select(.shell_outcome.success == false)`; handle cancelled and unavailable
outcomes separately through lifecycle `status` and object absence.

The projector never parses rendered output or display text. Cancellation,
unresolved/source-not-selected terminals, synthetic background placeholders,
malformed or contradictory maps, unavailable legacy fields, and non-shell
calls omit the object. A legacy structured map with an exit `status` but no
reason is treated as `exit` only for final foreground/background result
payloads and only when the `termination_reason`, `timed_out`, and `signal` keys
are all absent; error details never use this fallback. Missing data is never
converted to `unknown`.

The accepted field matrix is exact:

- `exit` requires `exit_code` and forbids a signal or true timeout.
- `timeout` requires `timed_out: true`; it preserves any recorded exit code or
  signal.
- `signal` requires `signal`, forbids a true timeout, and preserves a recorded
  exit code.
- `start_error` is accepted only from structured foreground/background tool
  error details and preserves otherwise well-typed optional fields (the current
  producer normally records no exit code, signal, or true timeout).
- Explicit `unknown` is accepted as the producer's known classification,
  preserving any otherwise well-typed exit code, signal, or true timeout; it is
  never a malformed-data fallback.

Final `ProviderToolResult` and `ToolBackgroundResult` payloads map to
`source: tool_result`. `ProviderToolError` and `ToolBackgroundError` details map
to `source: tool_error_details`. Wrong types, duplicate recognized text fields
(`status`, `signal`, `timed_out`, or `termination_reason`), unknown reason
strings, contradictory combinations, and integers outside signed 32-bit range
omit the whole object without failing trace export. Unknown keys, including
duplicates, are ignored because journal decoding has already erased some raw
CBOR key distinctions and they cannot affect this projection.

JSONL writes one header followed by independently parseable items. TOON writes
the same header fields, then a strict `items[N]:` counted array in the same
item order. Optional fields below are absent unless their stated evidence
survives:

TOON preserves strings directly when its grammar can round-trip them. Otherwise
it replaces the whole field with standard padded Base64: `call_id_base64`,
`message_id_base64`, `command_base64`, `text_base64`, or `output_base64`
decodes to the UTF-8 bytes of the corresponding JSONL string.
`arguments_json_base64` decodes to the complete
compact JSON encoding of `arguments`; consumers must Base64-decode and then parse
that whole JSON value. A record never emits both the direct and Base64 spelling
of one field. Base64 is a framing adaptation only and does not change semantic
record parity with JSONL.

```text
header:
  schema="tau.agent_trace_compact", schema_version=0, record_type="header",
  root_agent_id, included_agent_ids[], content="bounded"|"full",
  time_unit="microseconds",
  absolute_time="unix_epoch_microseconds_at_journal_append_invocation",
  timing_basis="producer_wall_clock_at_observation",
  causality="explicit_observation_refs_only"

common item:
  at_us, [recorded_at_unix_micros], agent_id, journal_seq, [item_index]

call:
  record_type="call", call={declaration,item_index},
  call_id, tool, arguments, status
  [command] [terminal] [terminal_resolution] [cause]
  [output,output_complete] [output_bytes,output_lines]
  [shell_outcome={source,success,termination_reason,
                  [exit_code],[signal],[timed_out=true]}]
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

assistant_message:
  record_type="assistant_message", agent_prompt_id, [phase],
  text_bytes, text_lines, text, text_complete

assistant_reasoning:
  record_type="assistant_reasoning", agent_prompt_id, reasoning_kind,
  text_bytes, text_lines, text, text_complete

message_sent:
  record_type="message_sent", message_id, sender_id, recipient_kind,
  [recipient_id], [recipient_session_id],
  text_bytes, text_lines, text, text_complete

message_received:
  record_type="message_received", message_id, sender_id, [sender_session_id],
  recipient_id, text_bytes, text_lines, text, text_complete
```

Resolution fields use `resolved` when the selected endpoint belongs to the same
agent journal and `source_not_selected` when it is unavailable to that
journal-local relationship. The latter includes selected endpoints in another
agent journal. Such relationships never produce elapsed intervals or transfer
terminal status/output ownership. Exact-wait targets outside the relationship's
journal project as `exact_unresolved`; plural exact targets do so as
`exact_all_unresolved`.

`ToolCallRef` always serializes as
`{"declaration":"<32-lowercase-hex>","item_index":<u32>}`. Observation references
use the same 32-digit encoding. Ordinary JSON-compatible arguments stay ordinary
JSON; values that JSON cannot preserve use the native tagged-CBOR shapes documented
above.

For example, a completion-delivering wait links rather than duplicates output:

```json
{"at_us":0,"recorded_at_unix_micros":1700000000000000,"agent_id":"agent-a","journal_seq":1,"item_index":0,"record_type":"call","call":{"declaration":"11111111111111111111111111111111","item_index":0},"call_id":"source","tool":"shell","arguments":{},"status":"ok","terminal":"22222222222222222222222222222222","terminal_resolution":"resolved","cause":{"kind":"completed"},"output":"done","output_complete":true}
{"at_us":20,"recorded_at_unix_micros":1700000000000020,"agent_id":"agent-a","journal_seq":5,"record_type":"relationship","relationship":"wait_settlement","observation_id":"55555555555555555555555555555555","wait_observation":"77777777777777777777777777777777","wait_call":{"declaration":"33333333333333333333333333333333","item_index":0},"registration":"active","registration_ref":"44444444444444444444444444444444","wait_terminal":"66666666666666666666666666666666","wait_terminal_resolution":"resolved","outcome":"completion_delivered","source_call":{"declaration":"11111111111111111111111111111111","item_index":0},"source_phase":"background","output_ref":"22222222222222222222222222222222","envelope":"original_tool_call_id_header","source_resolution":"resolved"}
```

Only assistant-role `Message` items and displayable `ReasoningText` items from a
canonical provider finish become semantic output. Ordered assistant text parts
are concatenated without a separator. Opaque provider reasoning, raw provider
JSON, unknown provider items, compaction items, and streaming updates never
become compact semantic text.

Only explicit `AgentMessageKind::Message` facts become message items; automatic
watch/status traffic remains excluded. A provider-declared `message` tool
`call` is the assistant's request or attempt, including failed attempts.
`message_sent` is the canonical accepted sender-side fact. Durable storage has
no call-to-message ID link, so the projector neither guesses one nor
deduplicates by equal sender, recipient, text, or time. `message_id` is the sole
explicit correlation between available sent and received directions.

Lite mode emits at most the first 4 KiB of UTF-8 for each semantic text and tool
output without splitting a code point. Exact `text_bytes`/`text_lines` and
`output_bytes`/`output_lines` describe complete canonical content, while
`*_complete` says whether the emitted spelling is complete. Full mode emits
complete text and output.

Compact trace identity and timestamps describe the captured journal facts, not
the session currently containing or exporting those journals. Loading or
importing a journal must not rebind historical agent, prompt, message,
endpoint-session, sequence, or timestamp fields for trace projection.
`root_agent_id` identifies the requested journal root and
`included_agent_ids` identifies captured journals; descendant inclusion is
selection, not item identity. Absolute samples come from the original host and
are not commit, execution, delivery, wire, or synchronized distributed time.

Use `--include-descendants` for the selected creator-owned workflow. Compact
lite exposes complete tool arguments and up to 4 KiB each of unredacted
assistant prose, displayable reasoning, explicit message text, and tool output.
Full exposes complete text and output. Reasoning/messages can contain secrets,
private communications, user data, or model-derived sensitive content; paired
directions can duplicate the same body. Absolute timestamps, membership, and
agent/session/message/prompt IDs expose sensitive activity metadata. Treat
either artifact as sensitive.


## Content-free performance JSON Lines

`agent-performance-jsonl` projects ordinary prompt accounting plus content-free
tool/background, typed wait, outer-turn, and standalone-compaction lifecycle.
It never emits prompt or response content, errors, tool names, arguments,
results, model parameters, or provider bodies. Agent, prompt, and
provider-qualified model IDs, descendant membership, activity timing, token
counts, cache reuse, cost, and work patterns remain sensitive metadata.

The first row uses schema `tau.agent_performance`, version `0`, and contains the
root/included agent IDs, `time_unit: "microseconds"`,
`timing_fidelity: "recorded_at_wall_clock_append_invocation_interval"`, and
`content_included: false`. Each included agent then emits occurrence rows ordered
by authoritative start journal sequence, then stable family/key, followed by one
`agent_summary`; agents remain in lexical order. `--include-descendants` uses the same authenticated creator scope
and snapshot as every other format.

```text
header: schema, schema_version, record_type, root_agent_id,
        included_agent_ids, time_unit, timing_fidelity, content_included
provider_prompt: record_type, agent_id, agent_prompt_id, model, journal_seq,
                 optional terminal_journal_seq,
                 optional at_us, optional terminal_at_us,
                 optional recorded_at_wall_elapsed_us, terminal_present,
                 optional prompt_sent_tokens,
                 optional prompt_cached_tokens,
                 optional response_received_tokens,
                 optional estimated_api_cost_picodollars
tool_call: record_type, agent_id, call, journal_seq, status,
           optional dispatch_at_us/cause/backgrounded_at_us/backgrounded_journal_seq,
           optional terminal_at_us/terminal_journal_seq,
           optional dispatch_to_backgrounded_us,
           optional backgrounded_to_terminal_us,
           optional dispatch_to_terminal_us
wait: record_type, agent_id, wait_call, journal_seq, optional observed_at_us, mode,
      registration, outcome, optional terminal_journal_seq/terminal_at_us,
      exact mode: target_call
      exact_all mode: target_calls
      activating_input mode: effective_timeout_minutes
      completion_delivered: source_call, source_terminal, source_phase, envelope,
                            optional completion_to_delivery_us
      completions_delivered: ordered sources array of source_call,
                             source_terminal, source_phase, and envelope
      interrupted_by_activation/input_available: activation,
                                                 optional activation_kind/
                                                          activation_to_wait_terminal_us
      rejected: rejection_reason
      active registration: optional active_wait_us
outer_turn: record_type, agent_id, outer_turn_id, agent_prompt_id,
            journal_seq, optional started_at_us, status, optional terminal boundary,
            optional recorded_at_wall_elapsed_us,
            optional automatic_compaction_decision_present
standalone_compaction: record_type, agent_id, transaction_id,
                       compact_prompt_id, trigger, journal_seq,
                       optional started_at_us, status,
                       optional failure_reason/terminal boundary/
                                recorded_at_wall_elapsed_us,
                       attempt_count, attempts
standalone attempt: agent_prompt_id, logical_attempt, model,
                    accounting_journal_seq, optional accounting_at_us,
                    corrected, output, usage_known,
                    optional prompt_sent_tokens/prompt_cached_tokens/
                             response_received_tokens/
                             estimated_api_cost_picodollars
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
ordinary-provider start and inclusion evidence. Its usage authority remains only
canonical `provider.response_finished`. Standalone attempts use only
`provider.standalone_execution_accounted`; a matching
`provider.standalone_execution_accounting_corrected` replaces the awaiting
cancellation sample in the same `(agent_prompt_id, logical_attempt)` entry.
`agent.compacted` token fields are lifecycle evidence and are never added as a
second usage total.

Tool and wait rows use only typed durable call/observation references. Missing
referenced terminals remain `source_not_selected`, `unresolved`, or incomplete;
the projector does not infer them from adjacency or parse provider arguments.
Activating-input waits expose only the durable effective timeout, not the
requested timeout. Standalone trigger and failure fields are categorical, and
attempts contain normalized usage/cost without provider rates or bodies.
Tool `cause` is the serialized `ToolTerminalCause`: `{"kind":"completed"}`,
`tool_error`, `provider_disconnected`, `lifecycle_teardown`,
`restart_repair`, or `unknown`; cancellation additionally carries its durable
request observation. Wait `mode`, `registration`, `outcome`, `source_phase`,
`envelope`, `activation_kind`, and `rejection_reason` use their snake-case
protocol enum values. `source_terminal` and `activation` are observation IDs;
`source_call` and `target_call` are `ToolCallRef` objects.

The optional
`recorded_at_wall_elapsed_us` and its summary sum compare the journal records'
wall-clock append-invocation timestamps. They are not durable commit time,
provider wire/model latency, or exact execution time, and intervals can overlap.
Zero timestamps are unavailable; decreasing clocks omit the interval. Relative
offsets are absent when no nonzero trace origin exists. Missing terminals stay
explicitly incomplete. Duplicate lifecycle/terminal facts for one agent/prompt
correlation fail projection rather than selecting one.

Projection retains compact content-free identities, categorical lifecycle facts,
and accounting counters until that agent's rows are emitted. It drops prompt,
tool, response, error, model-parameter, endpoint, rate, and cumulative-token
payloads while streaming the journal. Pathological identifier cardinality or
length can still exhaust memory.
