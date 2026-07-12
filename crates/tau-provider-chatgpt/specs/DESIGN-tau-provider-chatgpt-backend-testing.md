# DESIGN-tau-provider-chatgpt-backend-testing: Backend transport behavior is covered by focused local tests

Status: inferred

This crate owns ChatGPT/Codex HTTP/SSE parsing, WebSocket turn and pool
behavior, transport selection, the no-HTTP-fallback policy for
WebSocket-capable configs, provider-cache key derivation, and provider-specific
retry/error mapping. Backend transport behavior should be tested here with
focused unit tests and local fakes rather than duplicated in
`tau-ext-provider-builtin`.

For models whose resolved config advertises WebSocket support, that support is a
routing commitment rather than a speculative optimization. WebSocket
capability/limit failures and exhausted retryable WS failures must surface as
provider errors for the turn instead of silently retrying the same turn over
HTTP/SSE.

WebSocket changes should cover observable turn/pool contracts such as pool-key
identity, reservation/release behavior, reconnect behavior, provider-stream
idle timeouts, cancellation returning typed `LlmError::Canceled`, and abort
wakers waking blocked turn waits without relying on short receive polling.
Parser and streaming changes should keep using focused event/delta/snapshot
regression tests, with broader provider response streaming guidance in
`../../docs/testing.md`.
