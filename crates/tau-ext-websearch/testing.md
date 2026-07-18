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
