# ARCH-tau-provider-chat-completions: tau-provider-chat-completions architecture

Provider output is constrained by [SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).

This crate contains only the OpenAI-compatible Chat Completions wire adapter:
request construction, HTTP/SSE transport, stream parsing, accumulation,
classification, and replay conversion. Serialized profiles, OpenRouter
discovery, model publication, public response sampling, and harness event writes
belong to `tau-ext-provider-builtin`.

Each dispatched ordinary-inference or local-summary request has a five-minute
semantic-idle deadline and a non-renewable thirty-minute absolute deadline.
Only newly accepted nonempty assistant text, reasoning text, tool name, or tool
arguments renews idle time. SSE comments, empty JSON or semantic fields, usage,
status, identifiers, and other content-free activity cannot extend the request.
Cancellation remains checked before deadline classification, and deadline
failures retain the established transport retry classification.

## Exact route compatibility

The extension selects which Tau reasoning efforts a configured model publishes
and whether the adapter lowers them with OpenAI spellings or literal spellings.
OpenAI lowering folds extended levels to `high`; literal lowering preserves
`xhigh`. An omit-wire policy publishes one fixed effective server-side effort
without sending `reasoning_effort`; absence of an effort capability publishes
only non-reasoning operation. Provider-specific template switches such as Qwen's
`enable_thinking` remain non-conflicting `extra_body` members.

Transcript replay independently selects `reasoning_content`, `reasoning`, or
both aliases. Qwen routes use both so current vLLM schemas and compatible servers
receive preserved thinking on tool continuations.

An opted-in route may require all system authority to be the one initial system
message. The adapter rejects later System or Developer transcript messages for
that route before dispatch. Stream termination accepts absent/null finish
reasons and the known `stop`, `tool_calls`, and `length` values. Any other
non-null finish reason is a terminal protocol error rather than successful
completion with unknown semantics.

## Typed OpenAI prompt-cache lowering

The extension may opt one exact configured route into typed OpenAI prompt-cache
controls. The adapter never infers this capability from a provider, endpoint, or
model name. Every selected route sends an agent-derived key and explicit `30m`
options. Implicit mode sends no content marker and retains provider-selected
automatic behavior. Explicit mode marks the end of a non-empty system message
with the documented content-block breakpoint. It does not mark conversation or
tool suffixes, so it does not implicitly write a volatile suffix. The standalone
local compactor preserves these ordinary cache fields and appends its instruction
after the unchanged ordinary message prefix. Opaque `extra_body` cannot collide
with these typed top-level members.

## Local summary response validation

Standalone local-summary output uses a compact-only validator over original
stream events before the ordinary compatibility projection can discard unknown
fields or collapse terminal shape. It requires one final `stop`, one bounded
nonempty narrative, and independently bounded optional reasoning; tool calls,
opaque or extra semantic fields, multiple choices, and post-terminal output
reject the attempt. The compact-only state enforces each semantic channel's
selected byte limit before appending a delta and rechecks the completed
projection before release. Ordinary inference keeps its broader parser behavior.
The extension sampler exposes only content-free response statistics and existing
status/activity signals while this validation is pending. It never emits local-summary text or reasoning as a
transient delta. The backend returns its validated ordinary output projection to
the built-in extension, whose terminal validator alone wraps the accepted
narrative in the private extension-to-harness envelope. Invalid and canceled
attempts release no semantic output.

## Cache telemetry route capabilities

The adapter parses cache counters only after the extension selects
`AttemptCompat.cache_usage` for the exact route. DeepSeek hit/miss counters
therefore never become observations merely because a model or endpoint name
looks like DeepSeek. Any selected cache schema requires
`AttemptCompat.stream_options`, which requests the supported terminal streamed
usage member. OpenRouter selects the documented OpenAI-compatible read/write
shape, but its selected upstream can vary; those observations have unknown
residency and never establish a cache policy, renewal, or keepalive operation.

The exact attempt compatibility independently controls `tool_choice`. When the
route omits that control, Auto still sends native tool definitions and relies on
the route's default, while None removes the definitions and selector together.
Parallel-call control is emitted only when the route supports it and the final
request still contains tools.

## Function-call argument replay identity

Chat Completions providers expose function-call arguments as JSON text. The
stream parser must keep the exact provider string in
`ToolCallItem.raw_arguments_json` while also parsing it into
`ToolCallItem.arguments` for validation and tool dispatch.

Assistant tool-call replay must prefer `raw_arguments_json` when present so
provider-visible history preserves argument key order, whitespace, and numeric
spelling. Serializing parsed CBOR is only a fallback for old persisted records or
records that never had provider-wire JSON.

Streamed assistant text and reasoning text are emitted as append deltas only.
Providers publish public content-free byte/duration response stats on
`provider.response_updated_reported.response_stats`. The stats count backend response
bytes received by the provider transport before semantic parsing; they do not
carry provider content and are not transcript data.

The backend reports its dispatch instant immediately before the request send is
first polled and exposes a dedicated timing predicate over accepted typed state.
That predicate includes non-empty assistant/reasoning text and tool name or
arguments, while excluding transport bytes, ids, empty state, and buffered
reasoning delimiters. The extension sampler captures the first qualifying
callback before cadence filtering.

The optional private backend-stage trace measures production request lowering,
the existing single serialization, request dispatch, first decoded body chunk,
decoder work, and first semantic qualification at these same boundaries. It
uses only content-free scalar facts and does not alter public response sampling.

Streaming parsers may receive upstream chunks at arbitrary cadence, but Tau
protocol updates are sampled. The extension response sampler starts when the
finite backend attempt begins. Received stream data advances backend-owned,
prompt-local response byte counters before semantic event handling, while parsed
chunks update the typed progress view. The extension's rate-limited emitter writes
the first non-empty `provider.response_updated_reported` sample as soon as streamed output
is observed, then writes later non-terminal samples only on one-second response
deadlines; later byte changes never bypass that cadence. Each public
`response_stats` pair uses `previous` = the last provider sample actually
emitted for the prompt and `current` = the new cumulative sample. A terminal
flush is the other normal bypass and is allowed immediately before the provider
prompt closes. The harness validates provider ownership and broadcasts these
stats unchanged; UI clients render them directly.

## Transcript replay boundary

Chat Completions replay reconstructs provider-visible history from Tau's
semantic transcript, not from an opaque provider-wire transcript snapshot. Tau
preserves the Chat Completions `messages[]` meaning it needs to continue the
conversation:

- message roles and visible assistant/user/system text;
- assistant reasoning text when a compatible provider exposes it;
- assistant tool calls and terminal tool results;
- raw function-call argument JSON strings, via `ToolCallItem.raw_arguments_json`,
  because providers observe the argument string spelling during replay.

Tau does not currently preserve arbitrary provider-specific assistant-message
fields that are outside those semantics, such as unknown `message` object
members returned by a particular OpenAI-compatible server. Those fields are not
part of the shared Chat Completions transcript contract in Tau. Add an opaque
sidecar only when a concrete provider proves that a specific field is necessary
for replay, cache identity, or correctness; until then, typed semantic transcript
items remain the source of truth.

## Logical retry ownership

This crate executes one finite attempt for `tau-ext-provider-builtin`; it does
not own logical-prompt retry sleeps or attempt limits. Retryable and unknown
remote failures return a structured decision to the built-in provider runtime.
That runtime clears tentative output, releases its bounded worker slot, parks
the logical prompt in the process-lifetime scheduler, and reloads the mutable
profile before a later attempt. Proven deterministic request failures remain
terminal. Cold restart does not replay ambiguous in-flight requests.

Cancellation is prompt-scoped. Active request and stream waits must observe the
caller's cancellation source; delayed cancellation belongs to the scheduler.

Chat Completions prompt traffic uses async `reqwest` on an attempt-local Tokio
runtime and the provider process's immutable outbound policy. Reqwest does not
rediscover environment proxies or follow redirects; proxy and target TLS share
the platform verifier plus optional additive custom CA. Header and body futures
are polled with the prompt cancellation source;
dropping a canceled future aborts its connection without detaching work outside
the provider concurrency permit. Profile/model discovery uses the same
asynchronous transport and immutable policy, but is not a logical prompt attempt.

## Tool definitions

Chat Completions publishes Function-only model tool support. Request conversion
is fallible and rejects any non-Function definition as an invariant violation;
it must never silently omit one. Configured model capability is exactly empty or
Function-only, and parallel support is valid only for a Function-capable model.

## Typed image tool-result lowering

The adapter lowers typed images only when the extension marks the exact attempt
model as accepting native image tool results. It emits one `role: "tool"`
message with the original `tool_call_id` and multimodal `content`: normalized
text first, then `image_url` parts containing high-detail canonical data URLs.
This is llama.cpp's documented OpenAI-compatible multimodal tool-result shape;
the OpenAI Chat Completions schema itself promises only text parts for tool
messages, so compatible routes must opt in explicitly rather than inheriting a
provider-wide or model-name-derived default.

Image bytes and expanded data URLs share the same 24 MiB and 32 MiB aggregate
request bounds as Tau's Responses lowering. An over-limit image becomes a
bounded omission part. A route without the attempt capability retains a
byte-free text omission marker. Opt-in provider request diagnostics recursively
replace image data URLs with a fixed omission marker before persistence; only
the live outbound request body, not its debug-capture projection, contains the
canonical image data URL.

## Terminal request rejection

Canonical OpenAI-compatible identifiers are extracted in explicit root,
`error`, then `response.error` code/type order for HTTP envelopes. Streamed
errors retain their provider-specific `error.code`, `error.type`, and exact
`error.metadata.error_type` spellings. Across either family, context exhaustion
outranks transient identifiers, known transient identifiers outrank opaque
ones, and provider prose or recursively nested metadata never classifies a
failure. Operation-specific policy then maps this normalized evidence:
context-window rejections are typed terminal failures, deterministic request
4xx responses are terminal, and explicit transport, throttle,
authentication-repair, and server failures remain retryable.

Each finite operation carries its real one-based scheduler attempt, actual wire
dispatch count, backend reachability, and sticky semantic progress. Terminal
reports omit backend metadata when local request construction or route
resolution fails before egress. Private request, response, and HTTP-error
captures include operation plus logical/wire correlation, and failure capture
is finalized once after transport returns.

Retryable classifications feed the shared scheduler's closed structured status;
the harness, not this adapter, owns watcher correlation and fanout.

This crate sends prompt/tool context to OpenAI-compatible Chat Completions
providers and parses provider responses back into Tau stream state. Treat
upstream responses and diagnostics as crossing an external-provider trust
boundary.

Explicitly enabled durable-session request, successful-response, and HTTP-error
captures are serialized by this adapter and submitted to the shared
[`tau-provider`](../../tau-provider/specs/ARCH-tau-provider.md) bounded writer.
Submission never implies persistence; background
zstd compression, filesystem failures, overload, or process exit may omit these
best-effort diagnostics without delaying provider/UI work.

## Streaming status boundary

Providers must not copy raw streamed text, reasoning, or function-call argument
chunks into status text, logs, notices, or UI-only diagnostics. Provider response
stats are public, content-free metadata submitted on transient `provider.response_updated_reported`
events: the backend owns the prompt-local byte counter and the extension owns
sampling and event writes. The extension may emit the first non-empty
previous/current sample promptly, emits later non-terminal samples at no more
than 1Hz, and may emit a final flush. The harness validates ownership and
broadcasts the stats unchanged. Stats must contain only byte counts, elapsed timing, and
routing metadata, never raw provider text, tool arguments, prompt text, or wire
payloads. Parsed `ToolCallItem.arguments` and `raw_arguments_json` remain the
provider/tool replay surface once the tool call is complete.

## Provider error authority

HTTP classification accepts exact identifiers from root `code`, root `type`,
`error.code`, `error.type`, `response.error.code`, and
`response.error.type`, in that envelope order. Streamed error classification
accepts `error.code`, `error.type`, and the well-known
`error.metadata.error_type` provider spelling. These paths accept the exact
`context_length_exceeded` identifier and the bounded retry-class identifier set
in `tau_provider::retry_policy::classify_error_code`.

New structured paths for terminal context or streamed identifier classification
may participate only after review documents their exact field path and bounded
accepted identifiers. Provider prose, unknown identifiers, and arbitrary or
unrecognized nested metadata are non-authoritative in those classifications.
Known accepted transient identifiers override deterministic HTTP status
classification and remain retryable.

Watcher-visible retry state never includes the upstream body or human error text;
only the provider's closed structured classification crosses into the harness.
