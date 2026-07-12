# DESIGN-tau-ext-websearch-runtime-safeguards: Runtime safeguards

Status: unconfirmed

The extension bounds concurrent provider calls. When all permits are occupied,
new calls fail fast with a busy `ToolError` so the protocol reader can continue
handling `Configure` and `Disconnect` messages. Successful MCP response bodies
and decoded model-visible text are capped separately. HTTP error bodies,
JSON-RPC error messages, and sanitized provider diagnostics are also bounded;
oversized JSON-RPC error messages are replaced with compact deterministic
diagnostics.
