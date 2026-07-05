# tau-provider-chatgpt architecture

This crate contains the ChatGPT/Codex provider transport implementation shared by the built-in provider extension. It owns request construction, Responses HTTP/SSE handling, persistent Responses WebSocket pooling, provider-cache key derivation, and provider-specific retry/error mapping.

## Prompt-cache identity

First-party ChatGPT/Codex prompt-cache keys are stable per provider base URL and durable target `AgentId`. Prompt provenance (`PromptOriginator`) is intentionally not part of the key: a target agent must stay on the same provider cache bucket whether a turn came from direct user input, extension-originated work, a manager relay, or an agent-to-agent message.

The legacy `share_user_cache_key` prompt flag is retained for persisted events and older providers, but this crate treats it as a no-op for cache-bucket selection. Any future cache-sharing behavior should be explicit agent metadata (for example, a reviewed `share_cache_from` design) rather than inferring cache identity from prompt provenance.

WebSocket pool keys must follow the same identity as request `prompt_cache_key` values so upstream thread/session headers and request bodies target the same cache bucket.

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
receive wakes promptly without reducing the 120 second provider-event timeout.

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
