# DECISION-tau-harness-watcher-provider-work: Structured watcher-visible provider work

Authority: confirmed, 2026-07-11, dpc

The harness projects provider work to watchers using closed structured categories
and bounded numeric facts rather than provider-authored prose. Harness ownership
prevents provider text, errors, account data, or prompt content from crossing the
watch boundary.

Per-subscription state retains at most 64 nonterminal prompt identities per turn
generation. This avoids churn during ordinary serial work while imposing a fixed
bound during malformed or unusually long tool turns; evicted work is deliberately
treated as fresh if it reappears rather than growing state without limit.

The complete lifecycle, fanout, replay, deduplication, and retirement behavior is
specified by [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md).
