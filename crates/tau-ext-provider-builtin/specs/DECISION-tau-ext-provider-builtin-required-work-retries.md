# DECISION-tau-ext-provider-builtin-required-work-retries: Required provider work retries

Authority: confirmed, 2026-07-12, user

Logical required work survives retryable finite attempts outside scarce workers.
Unknown remote failures retry; typed terminal request rejection ends the work.
Retry state is memory-only to avoid ambiguously replaying accepted requests, while
explicit `/retry` atomically transfers the exact delayed work once without changing
its ownership or accounting.

Shared cooldowns remain exact-generation scoped, and only an admitted successful
terminal probe or profile replacement releases them; quota display telemetry is not
scheduler authority. This accepts loss on process restart rather than risk duplicate
remote work. Exact states and cadence are specified by
[SPEC-tau-ext-provider-builtin-retry-scheduler](SPEC-tau-ext-provider-builtin-retry-scheduler.md).
