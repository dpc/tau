# SPEC-tau-ext-websearch-runtime-safeguards: Websearch runtime safeguards

Provider calls time out after 45 seconds. At most eight run concurrently; a
ninth fails immediately with a busy `ToolError` without waiting, leaving
Configure and Disconnect processing responsive. Disconnect may detach blocked
workers; workers may finish, but the reader never blocks waiting for a permit.

HTTP error bodies other than HTTP 429 are capped at 64 KiB. An HTTP 429 returns
the bounded generic advice `web service rate-limited the request; try again
later.` without reading or projecting the provider body. Successful MCP response
bodies are capped at 1 MiB before decode. Decoded provider text retains its
512 KiB pre-projection cap, and the complete framed, closed
`<tau_web_content>` result is independently capped at 512 KiB. Expansion from
exact-close replacement or visible-Unicode escaping counts toward that final
bound; oversize results fail clearly as a `ToolError` and are not truncated into
a different success contract. A JSON-RPC error above 512 KiB is replaced by a
compact deterministic diagnostic, and every final sanitized model-visible error
is capped to 512 KiB with a UTF-8-safe suffix. Endpoint redaction occurs before
the final cap.

Exa result count defaults to five and accepts only 1–100. Replay-marked
`ToolStarted` deliveries are ignored: they issue no provider request and publish
no result.

Provider URL and diagnostic safety is
[SPEC-tau-ext-websearch-provider-boundary](SPEC-tau-ext-websearch-provider-boundary.md).
