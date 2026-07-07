# tau-provider-chatgpt architecture

This crate contains the ChatGPT/Codex provider transport implementation shared by the built-in provider extension. It owns request construction, Responses HTTP/SSE handling, persistent Responses WebSocket pooling, provider-cache key derivation, and provider-specific retry/error mapping.

## Transport selection

`ResponsesConfig::supports_websocket` is the source of truth for ChatGPT/Codex
Responses transport routing. When it is true, Tau treats WebSocket as the
required transport for that model/configuration, not as a speculative
optimization before HTTP/SSE. Capability or limit failures from the WebSocket
path, and retryable WebSocket failures after the bounded outer retry/backoff
policy is exhausted, must surface as provider errors for the turn rather than
silently replaying the same prompt over HTTP/SSE.

HTTP/SSE remains the Responses transport for configs that do not advertise
WebSocket support and for the HTTP/SSE-specific request/debug/replay paths.

## Prompt-cache identity

First-party ChatGPT/Codex prompt-cache keys are stable per provider base URL and durable target `AgentId`. Prompt provenance (`PromptOriginator`) is intentionally not part of the key: a target agent must stay on the same provider cache bucket whether a turn came from direct user input, extension-originated work, a manager relay, or an agent-to-agent message.

The legacy `share_user_cache_key` prompt flag is retained for persisted events and older providers, but this crate treats it as a no-op for cache-bucket selection. Any future cache-sharing behavior should be explicit agent metadata (for example, a reviewed `share_cache_from` design) rather than inferring cache identity from prompt provenance.

WebSocket pool keys must follow the same identity as request `prompt_cache_key` values so upstream thread/session headers and request bodies target the same cache bucket.

Replay contributes to the same provider-visible cache identity. When replaying
assistant function calls, request construction must prefer
`ToolCallItem.raw_arguments_json` so object key order, whitespace, and numeric
spelling match the provider's original argument string. Serializing parsed CBOR
arguments is only a fallback for older persisted records that do not have the raw
sidecar.

The same replay rule applies to opaque Responses provider items. Reasoning,
compaction, and unknown output items should be stored with
`OpaqueProviderItem.raw_json` when the upstream event JSON is available, and full
transcript replay should prefer that sidecar over the parsed CBOR
`OpaqueProviderItem.value`.

Responses assistant `message` items also carry a replay sidecar. Tau keeps the
typed message text and `phase` as semantic truth, but the raw Responses item
preserves provider-owned ids, status, annotations, content-part boundaries, and
unknown fields that may affect server-side replay/cache behavior. Full
transcript replay should emit the raw item unchanged when its text and phase
already match the typed fields and the raw item validates as a Responses
assistant `message`; otherwise it may parse the raw item and update only
text/phase before sending it, or synthesize from typed fields when validation
fails.

Responses tool-call output items split semantic tool-call routing from provider
envelope fidelity. `ToolCallItem.call_id`, name, type, and arguments remain the
validated Tau fields used for dispatch and tool-result pairing, while
`ToolCallItem.responses_envelope` stores the provider item id/status and unknown
non-structured fields needed to replay `function_call` and `custom_tool_call`
items without changing provider-visible item identity. The sidecar's
`extra_fields` is a parsed CBOR map of JSON object members; it preserves values,
not raw JSON spelling/order, and replay ignores non-map values. Extra fields
cannot override rebuilt structured fields such as `id`, `status`, `call_id`,
`name`, `arguments`, or `input`. Full transcript replay must fall back to the
historical `fc_`/`ctc_` id synthesis when that sidecar is absent.

## Streaming provider output

Responses streams may deliver visible assistant text, reasoning summaries, large
function-call arguments, or custom-tool input during an agent turn. Providers
emit displayable assistant/reasoning append deltas and final tool-call items, but
do not publish public byte-progress metadata. For streamed function-call
arguments and custom-tool input, the provider sends the harness a private,
content-free `semantic_output.non_visible_output_bytes` snapshot that is
cumulative for the current provider prompt, not a per-update delta.

Provider response throughput samples are also private provider-to-harness
metadata. The sampler starts when the backend request is dispatched. Chunk reads
only update in-memory cumulative state and pending visible/non-visible deltas.
The provider writes non-terminal `provider.response_updated` samples only on
one-second response deadlines; byte changes never bypass that cadence. Each
private `response_stats` pair uses `previous` = the last provider sample actually
emitted for the prompt and `current` = the new cumulative sample. A terminal
flush is the only normal bypass and is allowed immediately before the provider
prompt closes. The harness strips private response metadata before subscriber
delivery and surfaces any public liveness display only through the compatibility
`agent.turn_stats_updated` projection without replacing provider byte/elapsed
semantics.

## Model metadata tags

ChatGPT/Codex model publication includes provider-owned capability tags such as
`shell:chatgpt` and `tools:custom-text`. These tags describe the model/backend
surface; the harness owns all policy that maps them to tool alternatives.

## WebSocket turn cancellation

The synchronous WebSocket turn loop treats cancellation as an event source rather
than a polling cadence. Callers pass a `TurnAbort` implementation that can both
answer `is_aborted()` and register a `TurnAbortWaker`. While a turn waits for
provider events, the registered waker sends `InboundEvent::AbortWake` through the
same inbound queue used by reader/writer transport events, so the blocking
receive wakes promptly without reducing the five-minute provider-stream idle
timeout. That timeout is per-turn and resets whenever the provider sends an SSE
`data:` event or WebSocket frame; SSE comments, heartbeats, and partial-line
byte trickles do not count as provider progress. Tau does not currently impose a
separate absolute turn-duration timeout for ChatGPT/Codex streams.

`AbortWake` is only a wake hint. The loop always calls `TurnAbort::is_aborted()`
after waking, and that check remains authoritative so stale or coalesced wake
hints cannot cancel the wrong turn. When cancellation is confirmed, the turn
returns `LlmError::HttpStatus(499, "cancelled by harness")`, matching the rest of
the provider cancellation path. The waker guard unregisters on drop so completed
turns do not leave callbacks that could enqueue stale wake hints into a pooled
socket's later turn.

The same `TurnAbort` waker seam is used while a prompt turn waits for a busy
same-key WebSocket pool reservation. The pool records an abort-wake generation
under its mutex and notifies its condition variable, so checkout waits only for
either the busy key to clear or an abort wake to change the generation. This
keeps a canceled queued same-key turn from later sending a stale request after
the active turn releases.
