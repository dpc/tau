# ARCH-tau-provider-chat-completions: tau-provider-chat-completions architecture

Provider output is constrained by [SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).

This crate contains OpenAI-compatible Chat Completions request construction,
stream parsing, provider-model publication helpers, and replay conversion shared
by built-in Chat Completions and OpenRouter-style profiles.

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
`provider.response_updated.response_stats`. The stats count backend response
bytes received by the provider transport before semantic parsing; they do not
carry provider content and are not transcript data.

Streaming parsers may receive upstream chunks at arbitrary cadence, but Tau
protocol updates are sampled. The provider response sampler starts when the
backend request is dispatched. Received stream data advances in-memory
prompt-local response byte counters before semantic event handling, while parsed
chunks update pending visible/non-visible deltas. The rate-limited emitter writes
the first non-empty `provider.response_updated` sample as soon as streamed output
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
it must never silently omit one.

## Terminal request rejection

Canonical OpenAI-style `error.code`/`error.type` context-window rejections are returned as typed
terminal failures before retry scheduling. Deterministic request 4xx responses are terminal,
while explicit transport, throttle, authentication-repair, and server failures remain retryable.

Retryable classifications feed the shared scheduler's closed structured status;
the harness, not this adapter, owns watcher correlation and fanout.

This crate sends prompt/tool context to OpenAI-compatible Chat Completions
providers and parses provider responses back into Tau stream state. Treat
upstream responses and diagnostics as crossing an external-provider trust
boundary.

## Streaming status boundary

Providers must not copy raw streamed text, reasoning, or function-call argument
chunks into status text, logs, notices, or UI-only diagnostics. Provider response
stats are public, content-free metadata on transient `provider.response_updated`
events: providers own the prompt-local byte counter, may emit the first non-empty
previous/current sample promptly, emit later non-terminal samples at no more than
1Hz, and may emit a final flush. The harness validates ownership and broadcasts
the stats unchanged. Stats must contain only byte counts, elapsed timing, and
routing metadata, never raw provider text, tool arguments, prompt text, or wire
payloads. Parsed `ToolCallItem.arguments` and `raw_arguments_json` remain the
provider/tool replay surface once the tool call is complete.

## Provider error authority

Terminal context classification trusts only canonical OpenAI-style `error.code` and `error.type`;
echoed nested fields and provider prose are not authoritative. Known canonical transient
identifiers override deterministic HTTP status classification and remain retryable.

Watcher-visible retry state never includes the upstream body or human error text;
only the provider's closed structured classification crosses into the harness.
