# DESIGN-tau-ext-provider-builtin-structured-retry-facts: Structured provider retry facts

Status: confirmed, 2026-07-11, dpc

Provider retries carry closed structured categories, saturating attempt counts,
and approximate bounded delays independently of human UI prose. Providers emit
only these safe facts alongside their local display status; the harness
validates prompt ownership and exclusively owns watcher snapshots and fanout.
