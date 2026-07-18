# DECISION-tau-ext-websearch-runtime-safeguards: Bounded fail-fast provider calls

Authority: unconfirmed

Websearch provider calls use bounded concurrency and fail fast when saturated so
the protocol reader remains responsive to Configure and Disconnect. Transport
bodies, decoded model-visible output, and sanitized diagnostics have separate
caps rather than sharing one late unbounded conversion.

This prevents slow or oversized hosted-provider work from blocking extension
control flow or flooding model context. The tradeoff is immediate busy errors
and deterministic replacement of oversized diagnostics.

Exact limits and lifecycle behavior are
[SPEC-tau-ext-websearch-runtime-safeguards](SPEC-tau-ext-websearch-runtime-safeguards.md).
