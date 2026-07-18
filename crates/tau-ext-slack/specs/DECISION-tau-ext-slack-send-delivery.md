# DECISION-tau-ext-slack-send-delivery: Bounded at-least-once Slack sends

Authority: confirmed, 2026-07-15, dpc

Slack sends are process/session-scoped at-least-once operations: one initial
post plus at most one byte-identical bounded retry. Tau has no durable outbox,
restart idempotency, or exactly-once claim. Remote success, fact commit, and tool
completion are not one transaction.

This bounds duplicate ambiguity while preserving useful transient recovery.
Exact reservation, retry, ordering, retirement, publication, and error behavior
is [SPEC-tau-ext-slack-send-delivery](SPEC-tau-ext-slack-send-delivery.md).
