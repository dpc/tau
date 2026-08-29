# SPEC-provider-response-streaming: Provider response streaming

## Record justification

Provider backends, extension samplers, the shared protocol, harness correlation,
and UI consumers jointly implement this streaming contract, so no single local
artifact can own it coherently.

Providers submit `provider.response_updated_reported` as transient, prompt-local
progress. After generic report commit and prompt-owner validation, the harness publishes
canonical `provider.response_updated`. Assistant text and reasoning use append-only
deltas; provider-authored status and retry diagnostics stay in the separate status
field. The first non-empty progress sample may be emitted promptly. Later nonterminal
output, status, and stats updates are sampled at most once per second per prompt, with
one immediate terminal flush permitted before closure.

Standalone local-summary attempts keep every semantic progress channel private
until the built-in provider extension validates the complete terminal shape. Their sampled
updates may carry the existing bounded content-free byte/timing statistics and
status/activity signals at their existing cadence; they carry no assistant text,
reasoning, tool, or opaque delta.
Successful validation releases the narrative exactly once in the private
`LocalCompactionNarrative` terminal envelope. Invalid and canceled attempts
release no content-bearing update. Ordinary inference streaming and opted-in
private provider debug capture are unchanged.

Providers accumulate arbitrary upstream chunks, backend response bytes, visible
text, compaction status, and non-visible semantic output independently of that
public cadence. For content-coded HTTP responses, the shared network policy
exposes decoded chunks, so existing body bounds and HTTP response-byte
accounting apply to decoded payload bytes. Tau does not measure or publish the
encoded-body byte count. The rate-limited emitter, not the parser's chunk
cadence, decides when to publish an update.

For ordinary inference, `provider.response_finished.output_items` is the complete
durable replacement and replay source. Successful local-summary compaction
instead consumes its private terminal envelope into the canonical
`agent.compacted` replacement window without publishing a provider response.
Consumers clear transient state when an attempt restarts or repetition is
rejected, and replace rather than append that state when the terminal response
arrives. A valid update observed after late subscription may create an
ellipsis-prefixed transient block for an otherwise unknown live prompt. Stale,
already-finished, or invalid prompt updates do not create transcript state.

Transient delivery does not require a UI to draw every accepted intermediate
sample. A renderer may immediately fold an already-delivered adjacent run for
one prompt, provided it preserves delta order and stats endpoints and does not
cross status, compaction, lifecycle, UI-control, or ordering barriers. This is a
consumer-local projection choice; it does not change provider cadence,
canonical publication, persistence, or wire delivery.

Providers own transport-byte accounting and publish content-free previous/current cumulative `response_stats`; `previous` is the last sample actually emitted. The harness validates provider source and prompt ownership, derives the public agent id from harness state, and broadcasts accepted updates without becoming the accounting authority. Provider-authored errors, bodies, headers, prompt text, tool data, account identifiers, and secrets never enter public stats.

Providers may also publish an immutable first-semantic-output duration in those
transient stats. It starts immediately before the finite attempt's first backend
send/enqueue and ends at the first synchronous accepted semantic-state
observation. Qualifying output is non-empty assistant text, non-empty
displayable reasoning/summary text, a material opaque reasoning item accepted
at item completion, a non-empty tool name, non-empty function-call argument
text, or non-empty custom-tool input. Transport bytes, response/call/item ids,
role markers, status, finish reasons, usage, quota, annotations, empty item
allocations/deltas, reasoning-summary delimiters without text,
compaction/control output, unknown provider items, rejected repetition deltas,
and tool execution progress/results/errors do not qualify. Scheduled retries reset the duration;
transparent pre-semantic repair within one finite attempt does not.

Capture precedes publication batching, and later samples repeat the immutable
value without changing cadence. Absence means unsupported or not observed.
Neither the harness nor durable finished output, journals, replay, snapshots, or
final turn stats derive or retain it.

Stats preserve each backend's response-byte accounting before semantic parsing.
HTTP backends count decoded response chunks exposed by the shared network
policy; non-HTTP backends retain their existing accounting. Stats exclude
prompts, encoded-body byte counts, tool execution output and results, UI
rendering text, and Tau UI/harness protocol bytes, and are never folded into
transcripts. The harness validates prompt ownership and cancellation before
deriving public routing identity and broadcasting the samples unchanged.

Report/canonical authority and terminal correlation are specified by
[SPEC-provider-execution-reports-and-canonical-facts](SPEC-provider-execution-reports-and-canonical-facts.md).

First-party providers apply a bounded exact-repetition guard to assistant text, reasoning summaries, and tool-argument deltas before acceptance. Detection clears transient output and terminates with the closed `repetition_detected` failure rather than treating repeated text as durable model output.

Watcher projection is separately governed by [SPEC-agent-watch](SPEC-agent-watch.md); recovery and compaction authority are governed by [SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md).
