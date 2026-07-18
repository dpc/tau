# DECISION-tau-ext-provider-builtin-bounded-prompt-workers: Bounded prompt workers

Authority: inferred

Finite provider attempts run on a bounded worker pool. The manual provider actor
uses enqueue-before-wake and drains ready inputs, cancellation is cooperative across
queued and active waits, and best-effort prewarm is separately finite and supervised
rather than consuming prompt-worker permits.

This bounds scarce execution while allowing logical work to outlive one attempt and
keeps the protocol actor responsive. Runtime ownership and flow are documented in
[ARCH-tau-ext-provider-builtin](ARCH-tau-ext-provider-builtin.md).
