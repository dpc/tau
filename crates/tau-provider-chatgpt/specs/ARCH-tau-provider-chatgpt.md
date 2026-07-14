# ARCH-tau-provider-chatgpt: tau-provider-chatgpt architecture

Provider output is constrained by
[SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).

## Account quota telemetry

This adapter owns the isolated ChatGPT `/wham/usage`, HTTP quota-header, and
WebSocket `codex.rate_limits` contracts. It normalizes only bounded pool/window
facts and preserves independent usage and timing observations; credentials,
account ids, credits, and provider prose never leave the in-process provider
boundary. Applicability and UI pacing are owned above this crate as specified
by [DESIGN-provider-quota-pacing](../../../specs/DESIGN-provider-quota-pacing.md).
The official WebSocket contract assigns a valid nameless `codex.rate_limits`
turn event to the canonical default `codex` pool. Explicit valid
`metered_limit_name` or legacy `limit_name` values take precedence, while a
JSON null is treated as absence. Every present non-null pool field must
normalize successfully, including a lower-precedence legacy field, or the
observation is rejected rather than falling back.

## Typed image tool output

GPT-5.6 Sol, Terra, and Luna on the ChatGPT Responses surface explicitly
publish image input and image tool-result support. Successful typed function
results lower to one `function_call_output` whose `output` array contains the
normalized `input_text` followed by `input_image` data URLs. Canonical binary
bytes remain in Tau; base64 exists only in the outgoing request. Responses Lite
omits `detail` after local high-detail preparation. Other model/routes project a
bounded omission marker and never receive bytes.

Normal inference, WebSocket, replay, and standalone compaction share this item
converter. Each request admits at most 24 MiB of canonical image bytes and 32
MiB of image-attributable data URLs. Debug request files and VCR matching
fixtures replace data URLs with metadata before persistence.

## Transport selection

`ResponsesConfig::supports_websocket` is the source of truth for ChatGPT/Codex Responses
transport routing. When it is true, Tau treats WebSocket as the required transport for
that model/configuration, not as a speculative optimization before HTTP/SSE. Capability
or limit failures from the WebSocket path and retryable WebSocket failures must surface
to the outer logical-prompt scheduler rather than silently replaying the same prompt
over HTTP/SSE.

HTTP/SSE remains the Responses transport for configs that do not advertise WebSocket
support and for the HTTP/SSE-specific request/debug/replay paths.

## GPT-5.6 Responses Lite

The ChatGPT/Codex GPT-5.6 family uses the upstream Responses Lite request contract. HTTP
requests carry the internal Responses Lite routing header, while WebSocket
`response.create` messages carry the equivalent per-request `client_metadata` marker so
pooled sockets remain reusable.

Responses Lite is incompatible with legacy inline `context_management`. Tau therefore
suppresses inline compaction context and trigger items on normal GPT-5.6 inference.
GPT-5.6 instead advertises standalone compaction: the provider sends a unary HTTP
`POST /codex/responses/compact` with the Lite header and lowering, and the harness installs
its output as one replacement-window boundary. Non-Lite models retain their existing
inline context-management behavior.

Responses Lite moves tool declarations and base instructions from the top-level request
fields into leading developer input items, disables parallel tool calls, and keeps
reasoning context across all turns. Hosted Responses tools are not part of this
contract; Tau's tools remain client-executed definitions. Chained Lite WebSocket deltas
omit the developer prefix already owned by the previous response. Full replay after
reconnect or compaction includes it again.

ChatGPT model metadata distinguishes the raw provider context window from the effective
window published to the harness. Server-side compaction thresholds derive from the raw
window, while UI usage and local context limits use the provider's 95-percent effective
ceiling.

## Prompt-cache identity

First-party ChatGPT/Codex prompt-cache keys are stable per provider base URL and durable
target `AgentId`. Prompt provenance (`PromptOriginator`) is intentionally not part of
the key: a target agent must stay on the same provider cache bucket whether a turn came
from direct user input, extension-originated work, a manager relay, or an agent-to-agent
message.

The legacy `share_user_cache_key` prompt flag is retained for persisted events and older
providers, but this crate treats it as a no-op for cache-bucket selection. Any future
cache-sharing behavior should be explicit agent metadata (for example, a reviewed
`share_cache_from` design) rather than inferring cache identity from prompt provenance.

WebSocket pool keys must follow the same identity as request `prompt_cache_key` values
so upstream thread/session headers and request bodies target the same cache bucket.

Provider-visible replay fidelity, sidecar validation, and typed semantic authority are
specified by
[SPEC-tau-provider-chatgpt-streaming-replay](SPEC-tau-provider-chatgpt-streaming-replay.md).

## Model metadata tags

ChatGPT/Codex model publication includes provider-owned capability tags such as
`shell:chatgpt` and `tools:custom-text`. These tags describe the model/backend surface;
the harness owns all policy that maps them to tool alternatives.

## WebSocket turn cancellation

The synchronous WebSocket turn loop treats cancellation as an event source rather than a
polling cadence. Callers pass a `TurnAbort` implementation that can both answer
`is_aborted()` and register a `TurnAbortWaker`. While a turn waits for provider events,
the registered waker sends `InboundEvent::AbortWake` through the same inbound queue used
by reader/writer transport events, so the blocking receive wakes promptly without
reducing the five-minute provider-stream idle timeout. That timeout is per-turn and
resets whenever the provider sends an SSE `data:` event or WebSocket frame; SSE
comments, heartbeats, and partial-line byte trickles do not count as provider progress.
Tau does not currently impose a separate absolute turn-duration timeout for
ChatGPT/Codex streams.

`AbortWake` is only a wake hint. The loop always calls `TurnAbort::is_aborted()` after
waking, and that check remains authoritative so stale or coalesced wake hints cannot
cancel the wrong turn. When cancellation is confirmed, the turn returns typed
`LlmError::Canceled`; remote HTTP 499 responses and provider-authored body text remain
retryable. Mutable URL, credential, account, or header construction failures return
`LlmError::ReloadableConfig` and retry after profile reload. The waker guard unregisters
on drop so completed turns do not leave callbacks that could enqueue stale wake hints
into a pooled socket's later turn.

The same `TurnAbort` waker seam is used while a prompt turn waits for a busy same-key
WebSocket pool reservation. The pool records an abort-wake generation under its mutex
and notifies its condition variable, so checkout waits only for either the busy key to
clear or an abort wake to change the generation. This keeps a canceled queued same-key
turn from later sending a stale request after the active turn releases.

## Tool definitions

The Responses adapter publishes and serializes both Function and Custom tool types.
Model metadata must continue to advertise both so harness prompt capability truth
matches the upstream request.

## Terminal request rejection

Responses transports classify canonical provider error codes before transport recovery.
In particular, `context_length_exceeded` is a typed terminal provider failure across
WebSocket, SSE, and non-2xx responses: it cannot reopen a cached socket, replay a full
request, fall back between transports, or enter the logical-prompt retry scheduler.
Unknown stream failures and explicit transient status/code classes retain their existing
retry ownership. Terminal context classification trusts only canonical Responses
envelope `code`/`type` fields; echoed nested fields and provider prose are not
authoritative. Known canonical transient identifiers retain precedence over
deterministic HTTP status classification.

Retry classification remains provider-owned and is exported only as a closed structured
category, saturating attempt, and bounded approximate delay. The harness validates
prompt ownership before projecting those facts to watchers; provider display text and
response bodies are never watcher data.

## External provider trust boundary

This crate sends prompt/tool context to ChatGPT/Codex endpoints and parses provider
responses back into Tau stream state. Treat upstream responses and diagnostics as
crossing an external-provider trust boundary.

## Streaming status boundary

ChatGPT/Codex output implements
[SPEC-tau-provider-chatgpt-streaming-replay](SPEC-tau-provider-chatgpt-streaming-replay.md)
under the workspace
[SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).
Provider-authored text and tool payloads never become status or response-stat metadata.

## Transient reply hints

Message-envelope `reply` attributes are capability hints, not durable authority.
Responses chaining must not preserve an older server-side rendering after route or
effective-tool liveness changes.
