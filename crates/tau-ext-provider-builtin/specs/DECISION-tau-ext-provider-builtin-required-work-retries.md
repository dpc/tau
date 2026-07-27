# DECISION-tau-ext-provider-builtin-required-work-retries: Required provider work retries

Authority: confirmed, 2026-07-20, dpc

## Decision

Logical required work survives retryable finite attempts outside scarce
workers. Unknown remote failures retry; typed terminal request rejection ends
the work. Retry state is memory-only.

Quota telemetry and usage-window reset estimates are informational rather than
scheduler authority, so bounded retries continue independently of them.

## Rationale

Memory-only retry state accepts loss on process restart rather than risk
duplicating ambiguously accepted remote work. Exact states and cadence are
specified by
[SPEC-tau-ext-provider-builtin-retry-scheduler](SPEC-tau-ext-provider-builtin-retry-scheduler.md).
