# DESIGN-tau-ext-websearch-testing-strategy: Testing strategy

Status: unconfirmed

Tests must not contact live providers. Use trait stubs for protocol dispatch,
argument validation, concurrency, and lifecycle tests. Use loopback HTTP servers
only when checking actual HTTP request/response behavior such as headers,
payloads, status handling, or body caps.

Lifecycle tests that start the extension over streams must shut it down
deterministically. Prefer sending `Disconnect`, releasing any blocked stub
workers, and draining expected results. Teardown paths must not panic on expected
harness-side pipe closure.

Coverage should include:

- tool registration and enabled-by-default policy;
- configuration parsing, endpoint validation, and endpoint application;
- provider argument forwarding and local argument rejection;
- HTTP header/body behavior without live network calls;
- SSE/JSON MCP decoding;
- response-size and tool-output limits;
- concurrency saturation and prompt disconnect handling;
- replay-marked tool deliveries being ignored so historical tool calls cannot
  rerun provider requests.
