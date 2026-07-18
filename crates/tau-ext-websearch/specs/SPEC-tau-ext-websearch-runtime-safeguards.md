# SPEC-tau-ext-websearch-runtime-safeguards: Websearch runtime safeguards

Provider calls time out after 45 seconds. At most eight run concurrently; a
ninth fails immediately with a busy `ToolError` without waiting, leaving
Configure and Disconnect processing responsive. Disconnect may detach blocked
workers; workers may finish, but the reader never blocks waiting for a permit.

HTTP error bodies are capped at 64 KiB. Successful MCP response bodies are
capped at 1 MiB before decode. Decoded model-visible text is capped at 512 KiB.
A JSON-RPC error above 512 KiB is replaced by a compact deterministic diagnostic,
and every final sanitized model-visible error is capped to 512 KiB with a
UTF-8-safe suffix. Endpoint redaction occurs before the final cap.

Exa result count defaults to five and accepts only 1–100. Replay-marked
`ToolStarted` deliveries are ignored: they issue no provider request and publish
no result.

Provider URL and diagnostic safety is
[DESIGN-tau-ext-websearch-provider-boundary](DESIGN-tau-ext-websearch-provider-boundary.md)
pending resolution of redirect enforcement.
