# Websearch testing

Tests never contact live providers. Use trait stubs for dispatch, validation,
concurrency, and lifecycle; use loopback HTTP only for actual wire headers,
payloads, status handling, redaction, and body caps. Lifecycle tests shut down
deterministically via Disconnect, release blocked workers, and drain expected
results; expected harness-side pipe closure must not panic.

Cover registration/default policy, endpoint parsing, rejection, and
application, provider argument forwarding and local rejection, no-Authorization
behavior, JSON/SSE decode, independent response/output/error caps, redaction,
saturation with responsive control handling, and replay suppression.
Production output lifecycle coverage blocks the real writer, exhausts the
64-frame detached FIFO, and requires one exact worker result/error after checked
admission resumes. Forced mandatory-write failure must exit the extension loop
without falsely publishing a terminal.

Display-state lifecycle tests must assert the same safe query/fetch target appears
on progress, success, error, and busy terminals. Exercise the Exa and Parallel
paths, including a configured tool-name prefix. Treat query/URL labels as
untrusted metadata: cover control/layout escaping, byte bounds that preserve whole
escaped units, fetch-host projection without URL userinfo/query secrets, and the
absence of configured provider endpoints or returned content.

Use loopback servers to verify that HTTP 429 takes the shared generic
rate-limit path for both hosted clients and never projects hostile, oversized
error bodies or endpoint secrets.

Successful-result coverage must exercise Exa search, Parallel search, and
Parallel fetch with exact canonical `<tau_web_content>` attributes. Adversarial
coverage keeps provider titles/URLs and attempted markup literal, replaces only
exact closing sentinels, makes unsafe Unicode visible, checks the exact final
512 KiB post-framing boundary
and oversize rejection, and proves identical preservation through Chat
Completions and Codex/Responses tool-result lowering.
