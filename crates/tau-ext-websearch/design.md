# Design decisions

This file records durable design decisions for `tau-ext-websearch`. Each decision
captures the current expected behavior and should be updated when provider,
configuration, runtime, security, or testing assumptions change.

## Provider boundary

Status: unconfirmed

`tau-ext-websearch` adapts hosted MCP web providers into Tau tools. Exa search is
enabled by default; Parallel search/fetch tools are registered but disabled by
default for explicit role opt-in. Provider output is untrusted model input.

Endpoint fields are ordinary extension config, but raw endpoint URL values are
log-sensitive because URLs can contain credentials in userinfo, query strings, or
fragments. Endpoint URLs are validated before use, userinfo is rejected, and
HTTPS is required except for loopback HTTP endpoints used by deterministic tests.
Logs must not print raw endpoint URLs. Provider transport diagnostics and
JSON-RPC error messages may become model-visible tool errors, so they must be
sanitized for configured endpoint echoes, request targets, query keys/values,
fragments, and userinfo before being returned.

## Runtime safeguards

Status: unconfirmed

The extension bounds concurrent provider calls. When all permits are occupied,
new calls fail fast with a busy `ToolError` so the protocol reader can continue
handling `Configure` and `Disconnect` messages. Successful MCP response bodies
and decoded model-visible text are capped separately. HTTP error bodies,
JSON-RPC error messages, and sanitized provider diagnostics are also bounded;
oversized JSON-RPC error messages are replaced with compact deterministic
diagnostics.

## Testing strategy

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
