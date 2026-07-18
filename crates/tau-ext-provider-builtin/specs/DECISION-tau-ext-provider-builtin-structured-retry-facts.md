# DECISION-tau-ext-provider-builtin-structured-retry-facts: Structured provider retry facts

Authority: confirmed, 2026-07-11, dpc

Providers emit only closed categories, saturating attempt counts, and bounded
approximate delays alongside local display status. The harness validates prompt
ownership and alone owns watcher snapshots and fanout. This separates safe
operational facts from provider-authored prose and avoids duplicate watch authority.

Backend classification follows
[DECISION-tau-provider-codex-retry-observability](../../tau-provider-codex/specs/DECISION-tau-provider-codex-retry-observability.md).
Exact fields and harness projection are specified by
[SPEC-tau-proto-provider-updates](../../tau-proto/specs/SPEC-tau-proto-provider-updates.md)
and [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md).
