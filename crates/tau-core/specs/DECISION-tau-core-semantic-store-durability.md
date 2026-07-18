# DECISION-tau-core-semantic-store-durability: Semantic store durability modes

Authority: unconfirmed

`AgentStore` and `SessionStore` support both durable streams and selected
memory-only streams. Memory-only stores fold the same live semantic facts and
provide same-daemon replay without creating reserved directories, sidecars,
locks, or event files. Durable replay is fail-closed: malformed framing,
sequences, identifiers, or semantic parent/event invariants return typed errors
instead of being ignored or partially folded.

This shared semantic boundary lets ephemeral agents and sessions behave normally
within one daemon lifetime without accidentally creating durable artifacts.
Strict durable validation avoids silently accepting corrupted or spliced state;
the tradeoff is that invalid journals block replay and require explicit recovery.

Store ownership and overlays are described by [ARCH-tau-core](ARCH-tau-core.md).
Derived agent listing state is described by
[DECISION-tau-core-agent-summary-checkpoints](DECISION-tau-core-agent-summary-checkpoints.md).
Persistence and replay behavior is specified by
[SPEC-tau-harness-session-state](../../tau-harness/specs/SPEC-tau-harness-session-state.md),
[SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md),
and
[SPEC-extension-published-message-facts](../../../specs/SPEC-extension-published-message-facts.md).
Architectural or externally meaningful changes remain governed by
[DECISION-persistence-and-extension-interface-change-approval](../../../specs/DECISION-persistence-and-extension-interface-change-approval.md).
