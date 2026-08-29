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

The backend replays the complete typed Responses transcript on every request.
It supports assistant text, plain `reasoning_text` reasoning, and Function
tools. Plain reasoning produces full displayable reasoning under the existing
thinking-visibility policy and a separate opaque durable item; replay skips the
display companion and emits the opaque item's required validated raw JSON
directly. Completed reasoning without exact raw JSON rejects before durable
output is formed; replay has no structured fallback or raw-less migration.
Encrypted, summary-only, malformed, or mixed reasoning remains unsupported. The backend
also preserves Responses assistant and function-call replay sidecars and never
sends `previous_response_id` or provider-side compaction controls. The
extension owns profile storage, model publication, retry scheduling,
cancellation policy, and protocol-event sampling.

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

Every request also lowers the harness-selected effective reasoning effort as
`reasoning.effort`. The public API spells Tau's `off` as `none`; the remaining
canonical levels (`minimal`, `low`, `medium`, `high`, `xhigh`, and `max`) pass
through directly.

An exact configured route may opt into legacy OpenAI automatic-cache retention
or explicit first-input-text caching. The adapter sends an agent-derived
`prompt_cache_key` and either retention or explicit options in the shared
HTTP/SSE and WebSocket request body. Explicit mode keeps top-level
`instructions` unchanged and marks only the earliest Tau-constructed
non-assistant `input_text` block. It is per-agent multi-turn cost control, not a
system-prompt boundary or cross-agent reuse; no eligible input fails before
egress. The legacy policy accepts the provider's automatic cache behavior and
any associated volatile-suffix/write-premium risk.
