# DECISION-tau-provider-chatgpt-cooperative-cancellation: Cooperative provider cancellation

Authority: inferred

ChatGPT/Codex transport, pool, connection, and prewarm waits use registered abort
wake sources rather than short polling or hard socket interruption. A wake is only
a hint; the typed local abort source remains cancellation authority, while remote
HTTP status or prose cannot impersonate cancellation.

This keeps cancellation prompt without weakening stream watchdogs or abandoning
shared-pool cleanup. Exact cancellation and deadline behavior is specified by
[SPEC-tau-provider-chatgpt-cancellation](SPEC-tau-provider-chatgpt-cancellation.md);
runtime ownership is documented in
[ARCH-tau-provider-chatgpt](ARCH-tau-provider-chatgpt.md).
