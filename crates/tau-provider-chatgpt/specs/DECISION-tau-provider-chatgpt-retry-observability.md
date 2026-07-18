# DECISION-tau-provider-chatgpt-retry-observability: Structured retry observability

Authority: confirmed, 2026-07-11, dpc

Provider adapters classify attempts and the shared scheduler publishes bounded,
safe structured retry facts independently of local UI prose. The harness, not the
provider, owns watch subscription, deduplication, and fanout. This keeps operational
visibility provider-neutral without exposing untrusted response text.

The provider-facing fact boundary is shared with
[DECISION-tau-ext-provider-builtin-structured-retry-facts](../../tau-ext-provider-builtin/specs/DECISION-tau-ext-provider-builtin-structured-retry-facts.md).
Exact fields and harness projection are specified by
[SPEC-tau-proto-provider-updates](../../tau-proto/specs/SPEC-tau-proto-provider-updates.md)
and [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md).
