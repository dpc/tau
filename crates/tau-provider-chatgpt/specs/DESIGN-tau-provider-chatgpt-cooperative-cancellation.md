# DESIGN-tau-provider-chatgpt-cooperative-cancellation: WebSocket cancellation is a cooperative wake source

Status: inferred

ChatGPT/Codex WebSocket turns run from synchronous prompt workers while socket IO
is owned by background Tokio reader/writer tasks. A turn waiting for upstream
events registers a `TurnAbortWaker`; cancellation sends an `AbortWake` hint into
the same inbound event queue as transport events. The turn then re-checks
`TurnAbort::is_aborted()` and returns typed `LlmError::Canceled` only when the abort source confirms cancellation. Remote HTTP 499/body text remains retryable.

This keeps cancellation prompt without shortening the provider-stream watchdog.
The default five-minute timeout is an idle timeout meaning "no upstream SSE
`data:` event or WebSocket frame arrived for this long", not "wake periodically
to poll cancellation". SSE comments, heartbeats, and partial-line byte trickles
do not reset it. Tau does not currently impose a separate absolute turn-duration
timeout for ChatGPT/Codex streams.

The shared WebSocket pool uses the same cancellation seam when a prompt turn is
queued behind an active same-key reservation. Checkout waits on the pool
condition variable until the busy key clears or a registered abort waker bumps
the pool's abort-wake generation. A canceled waiter must return typed `LlmError::Canceled` instead of starting a stale network turn after the earlier same-key turn
releases.

Fresh DNS/TCP/TLS/WebSocket upgrade work uses that same abort source and a
separate 30-second connection deadline. Cancellation races the upgrade through a
registered wake notification; timeout is a sanitized retryable transport error.
Either outcome abandons the same-key pool reservation before control returns to
the prompt scheduler. This connection deadline is independent of the
five-minute provider-frame idle watchdog.

Cache prewarm uses the same abort-waker contract but is independently finite:
the upgrade retains the 30-second connection deadline and the non-generating
response has a 30-second absolute deadline. Cancellation is rechecked before a
successful socket can return to the pool.
