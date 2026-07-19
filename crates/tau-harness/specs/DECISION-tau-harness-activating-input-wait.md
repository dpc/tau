# DECISION-tau-harness-activating-input-wait: Bounded same-turn activating-input waits

Authority: confirmed, 2026-07-12, user

The input form of `wait` may park one foreground tool call without concrete
owned background work, but only for a bounded deadline. It remains in the same
outer running agent turn and tool round; waiting does not invent a suspended or
idle lifecycle state and does not itself activate watchers.

Wait registration is runtime-only. The harness event loop arbitrates input,
cancellation, and timeout, and cold restore repairs the unresolved tool rather
than recreating a waiter. This avoids indefinite parking and durable waiter state.

Exact parameters, wakeup selection, consumption, and restore behavior are
specified by
[SPEC-tau-harness-activating-input-wait](SPEC-tau-harness-activating-input-wait.md).
