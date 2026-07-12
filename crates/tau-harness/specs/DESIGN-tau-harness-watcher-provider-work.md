# DESIGN-tau-harness-watcher-provider-work: Watcher-visible provider work

Status: confirmed, 2026-07-11, dpc

The harness projects provider work to watchers using closed structured categories and
bounded numeric facts rather than provider-authored prose. The complete operational
contract is specified by [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md).

## Rationale

Harness ownership prevents provider text, errors, account data, or prompt content from
crossing the watch boundary. Per-subscription state retains at most 64 nonterminal
prompt identities per turn generation: this avoids churn during ordinary serial provider
work while imposing a fixed bound during malformed or unusually long tool turns.
Capacity-evicted work is deliberately treated as fresh if it reappears rather than
growing state without limit.

Focused delivery-bookkeeping tests own generation, dedupe, terminal retirement, FIFO
capacity, and high-cardinality stress. Harness dispatch tests own fanout, durable facts
and replay, initial snapshots, and relation cleanup.
