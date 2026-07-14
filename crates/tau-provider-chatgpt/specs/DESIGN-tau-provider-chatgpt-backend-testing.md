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
Prewarm regressions should also cover a silent upgraded peer, duplicate
same-key admission, cancellation cleanup, invalidation racing late release, and
successful socket reuse.
Parser and streaming changes should keep using focused event/delta/snapshot
regression tests, with broader provider response streaming guidance in
`../../docs/testing.md`.

Changes to a default route or protocol surface require a capability matrix
covering direct function/custom calls, parallel generation, programmatic/code
mode, hosted tools, images/detail, reasoning continuity, compaction,
chaining/replay, HTTP/WebSocket, quota/retry/fallback, and profile/auth scope.
Golden requests cover every supported mode. Prompt tests must prove that no
unsupported capability is advertised, and the default mode must retain an
end-to-end multi-tool lifecycle test. Reference-client metadata is evidence,
not by itself a Tau requirement; compatibility modes remain explicit and never
become retry fallbacks without a separately approved design.
