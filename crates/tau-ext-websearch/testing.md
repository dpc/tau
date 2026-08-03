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
