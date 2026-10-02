# ARCH-tau-provider-responses: Public Responses backend

`tau-provider-responses` owns one finite API-key HTTP/SSE or WebSocket attempt
for the generic public Responses protocol. It is separate from both the generic Chat
Completions backend and the private ChatGPT/Codex WebSocket backend.
Canonical response metadata identifies it as `PublicResponses`; the legacy
`Responses` backend kind remains private ChatGPT/Codex and cannot acquire public
full-replay continuation authority.

Standalone local-summary sampling exposes only bounded content-free response
statistics and existing status/activity signals while the complete output shape remains unvalidated. The backend
returns ordinary typed output to the built-in extension, whose terminal validator
alone wraps one accepted narrative in the private extension-to-harness envelope.
Invalid and canceled attempts release no semantic output. Ordinary inference
streaming and opted-in private debug capture retain their existing behavior.

The backend replays the complete destination-projected transcript on every
request. The harness preserves portable message/tool semantics and admits opaque
material and wire sidecars only for known-compatible producing-provider origin,
under [REQ-best-effort-provider-switching](../../../specs/REQ-best-effort-provider-switching.md).
The adapter neither guesses origin nor repairs foreign JSON. An admitted
opaque compaction item is replayed from its exact raw JSON; the generic public
profile does not itself start native compaction.
It supports assistant text, completed reasoning items, and Function tools.
Plain `reasoning_text` produces full displayable reasoning under the existing
thinking-visibility policy and a separate opaque durable item; replay skips the
display companion and emits an admitted opaque item's required validated raw JSON
directly. Completed reasoning without exact raw JSON rejects before durable
output is formed; replay has no structured fallback or raw-less migration.
Opaque, summary-only, and encrypted reasoning produces only the opaque durable
item and is not projected as reasoning text. Documented combinations of summary,
encrypted content, and plain reasoning content retain the same rule: only
`reasoning_text` content is displayable, while an admitted raw item is replayed exactly.
Malformed identities, encrypted content, summaries, or content remain unsupported. The backend
also preserves admitted Responses assistant and function-call replay sidecars and never
sends `previous_response_id` or provider-side compaction controls. The
extension owns profile storage, model publication, retry scheduling,
cancellation policy, and protocol-event sampling.

Other provider adapters can supply an exact pre-lowered full-replay SSE request
through `PreparedSseRequest` and the prepared-attempt entry point. They own route
policy and retain responsibility for destination-origin admission; the seam
confers no new replay authority. Generic lowering remains available as a baseline,
and prepared bodies retain raw JSON rather than rebuilding opaque input items.
The seam rejects model/transport mismatches and response-id chaining. It reuses
the ordinary finite SSE transport, parser, terminal classification, capture and
accounting without changing generic or private Codex defaults.

A prepared ChatGPT plan-sharing request additionally requires
`response.completed`: legacy `response.done` and `[DONE]` aliases reject.
It also disables transport-level HTTP retries through an explicit prepared
request policy, without changing other adapters' client defaults.
The owning extension treats all plan-route failures as Error terminals, while
generic Responses and other prepared adapters retain their existing terminal
and retry behavior.

A prepared-request adapter may additionally select exact nested incomplete
reasons that terminate with `Error`, retaining only validated assistant prose,
terminal usage and response identity. This policy never retains ToolCall,
reasoning or opaque items, never retries, and grants neither Length continuation
nor context-overflow recovery. Grok selects `max_prompt_tokens` and
`max_time_limit`; ordinary generic requests select no such reasons. Reconciliation
and structural validation still precede publication, including empty-prose
terminals with usage. The existing extension report and canonical journal
represent these outcomes without a new backend discriminator or core schema.

Profiles select transport explicitly; omission retains the historical SSE
default. A WebSocket attempt opens a fresh connection, sends one
`response.create` envelope, and closes after the terminal event. Retry scheduling
therefore reconnects and replays the complete local transcript rather than
depending on connection-local continuation state. WebSocket selection never
falls back to SSE.

Both transports decode each bounded event's accepted JSON text once.
Assembler-bound events build a lexical index of exact value spans in that text
without interpreting JSON a second time. They feed the shared semantic
projection into one indexed response assembler.
It retains slots in ascending provider `output_index` order and projects only
the contiguous prefix beginning at zero, so a later item never appears before
an unresolved earlier item. A terminal `response.output` array authoritatively
replaces streamed slots in array/index order while retaining exact raw item
sidecars; a terminal without that array accepts only a contiguous accumulated
sequence. Invalid indices, gaps, and non-array terminal output fail the finite
attempt rather than inventing an order.

Canonical `response.incomplete` with exact reason `max_output_tokens` completes
the finite attempt as `ProviderStopReason::Length`. Both transports reconcile
and preserve validated partial output, terminal usage, and response identity;
the extension never retries the unchanged request. Ordinary inference retains
partial prose, suppresses truncated tool execution through the shared Length
terminal policy, and may use the single existing continuation only for
provider-native replay-safe reasoning-only output. Standalone local-summary
compaction records the incomplete response outside transcript context, never
accepts its partial narrative as a replacement window, and never retries it
automatically. Every other incomplete reason remains a provider failure. The
cross-component continuation and compaction contract is governed by
[`SPEC-compaction-and-context-recovery`](../../../specs/SPEC-compaction-and-context-recovery.md).

When the extension enables durable-session provider diagnostics, the adapter
selects the existing HTTP/SSE or WebSocket capture class. HTTP/SSE records the
final request at the `reqwest` send boundary. WebSocket records the exact
`response.create` envelope at its frame-send boundary, after connection and
upgrade work. A successful response is recorded only after terminal validation;
non-cancellation build, runtime, transport, HTTP/provider, parsing, and validation
failures produce bounded error metadata. Cancellation produces no error capture,
though a request capture remains when cancellation arrives after that request's
send boundary. Standalone local compaction follows the same durable-session diagnostic policy as ordinary inference; disabled diagnostics submit nothing.

Ordinary durable inference and local-summary standalone compaction also have independent startup-frozen scalar cache
diagnostics, defaulting to metadata. These reuse the shared bounded private
capture writer, not logs or journals. Every finite invocation has a random
capture-local attempt identity, including when metadata is opted out. Exact
requests retain a null actual wire-dispatch index at their unchanged submission
point; cancellation can still prevent dispatch afterwards. Only the diagnostic
dispatch row establishes actual index one. Later exact responses and failures
may carry that index. Local-summary records use `standalone_compaction` and the
existing prompt identity; their logical-attempt ordinal remains unavailable and
their harness provider attempt is copied only when the extension supplied it.

The scalar attempt end follows the backend's existing finite outcome before the
built-in extension performs final local-summary narrative validation. It
preserves allowlisted raw usage counters from parsed terminal JSON before
canonical normalization, including when later output validation fails. Other
failure paths may have no usage evidence. The extension supplies its typed
provider attempt locally; no independent logical ordinal is invented. Each
invocation observes at most one attempted send, full replay and no local repair
or connection reuse. Capture loss never changes execution or proves exhaustive
history, provider receipt, billing, residency, cache eligibility or canonical
compaction success.

Response snapshots retain at most 512 KiB and 4,096 raw event JSON values.
Serialization writes through a strict 1 MiB ceiling and replaces an oversized
record with content-free truncation metadata without first materializing the
oversized JSON. Before submission, the adapter recursively removes embedded image
data URLs from requests, replay sidecars, function extras, successful raw events,
and opaque HTTP error bodies. It also replaces exact occurrences of the untrimmed
API key actually dispatched, including JSON keys; a projected-key collision
fails closed by replacing that object. The shared writer keeps these best-effort
artifacts separate from logs, journals, UI events, and the typed transcript.

Each transport gives request dispatch, connection, and response-header work
five minutes. After successful headers, one response stream has a separate
ten-minute absolute deadline plus a five-minute semantic-idle deadline.
Accepted qualifying semantic output renews only the idle deadline; it never
extends the absolute deadline. Qualifying progress is a non-empty assistant or
displayable reasoning-text addition, a completed material opaque reasoning
item, a non-empty Function name, or non-empty Function arguments. Transport
bytes, SSE comments, WebSocket ping/pong/control frames, status and usage,
empty allocations/deltas, unknown events, and duplicate semantic state do not
qualify. Cancellation remains cooperative throughout every bounded wait.

The adapter emits one internal dispatch observation only after pre-dispatch
work succeeds: immediately before SSE first polls its request send, or
immediately before WebSocket enqueues `response.create` after connection and
upgrade. Build, runtime, capture, connection, upgrade, and canceled or failed
pre-dispatch exits emit none. The built-in extension uses this observation to
re-anchor its existing transient response-stat clocks and immutable
first-semantic-output duration under
[`SPEC-provider-response-streaming`](../../../specs/SPEC-provider-response-streaming.md);
it adds no event field or response semantics.

The separately selected private backend-stage trace observes request lowering,
the existing transport-specific serialization and capture, WebSocket
connect/upgrade, send or frame enqueue, first body chunk or text frame, decoder
work, and the same semantic qualification predicate. It emits only bounded
scalar facts on a dedicated process-local target; the disabled path creates no
trace state and takes no observation clock.

Numeric and disabled requests lower the harness-frozen effective native
reasoning selector as `reasoning.effort`; provider-default, fixed, and
unsupported selections omit it. The public API accepts the canonical native
levels `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`.

An exact configured route may opt into OpenAI cache mode and lifetime controls.
The adapter sends an agent-derived `prompt_cache_key` and
`prompt_cache_options: { mode, ttl }` in the shared HTTP/SSE and WebSocket
request body. Implicit mode sends no Tau content marker and accepts the
provider's automatic breakpoint behavior and any associated volatile-suffix/write
premium risk. Explicit mode keeps top-level `instructions` unchanged and marks
only the earliest Tau-constructed non-assistant `input_text` block. It is
per-agent multi-turn cost control, not a system-prompt boundary or cross-agent
reuse; no eligible input fails before egress. The retired legacy
`prompt_cache_retention` control is rejected rather than translating its `24h`
retention contract into the distinct `30m` TTL.
