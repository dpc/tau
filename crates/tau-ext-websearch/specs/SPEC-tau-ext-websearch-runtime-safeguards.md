# SPEC-tau-ext-websearch-runtime-safeguards: Websearch runtime safeguards

## Record justification

Concurrency, scheduling, transport deadlines, response bounds, cancellation,
and replay behavior span the extension loop, provider adapters, protocol
terminals, and deterministic tests, so no single implementation artifact owns
the complete safeguard contract.

One admitted composite call has a 45-second total deadline and tries at most
three configured providers sequentially, once each. Before each attempt, its
deadline is the remaining total divided by the remaining allowed attempts;
unused time carries forward. Rate limits, transport and provider failures,
rejections, invalid or oversize responses, projection failures, and
trimmed-empty text fail over. Local validation errors issue no attempts.

At most eight tool calls run concurrently; a ninth fails immediately with a busy `ToolError` without waiting, leaving
Configure and Disconnect processing responsive. Disconnect may detach blocked
workers; workers may finish, but the reader never blocks waiting for a permit.

HTTP error bodies other than HTTP 429 are capped at 64 KiB. An HTTP 429 returns
the bounded generic advice `web service rate-limited the request; try again
later.` without reading or projecting the provider body. Successful hosted response
bodies are capped at 1 MiB before decode. Decoded provider text retains its
512 KiB pre-projection cap, and the complete framed, closed
`<tau_web_content>` result is independently capped at 512 KiB. Expansion from
exact-close replacement or visible-Unicode escaping counts toward that final
bound; oversize results fail clearly as a `ToolError` and are not truncated into
a different success contract. A JSON-RPC error above 512 KiB is replaced by a
compact deterministic diagnostic, and every final sanitized model-visible error
is capped to 512 KiB with a UTF-8-safe suffix. Endpoint redaction occurs before
the final cap.

Cancellation prevents subsequent attempts. An already issued request through
the blocking transport can continue until its allocated attempt deadline and
may consume quota; its response is discarded when cancellation wins before the
serial protocol loop commits another terminal. Every issued failover attempt
may independently consume quota or incur charges. Multi-request adapters check
cancellation between requests, so a canceled You.com initialization cannot
proceed to its quota-bearing tool call.

Exa result count defaults to five and accepts only 1–100. Replay-marked
`ToolStarted` deliveries are ignored: they issue no provider request and publish
no result.

Provider URL and diagnostic safety is
[SPEC-tau-ext-websearch-provider-boundary](SPEC-tau-ext-websearch-provider-boundary.md).
