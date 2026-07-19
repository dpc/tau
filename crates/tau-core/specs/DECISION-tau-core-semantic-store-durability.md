# DECISION-tau-core-semantic-store-durability: Semantic store durability modes

Authority: unconfirmed

`AgentStore` and `SessionStore` support both durable streams and selected
memory-only streams. Memory-only stores fold the same live semantic facts and
provide same-daemon replay without creating durable artifacts. Durable replay is
fail-closed rather than ignoring or partially folding invalid journals.

The shared semantic boundary lets ephemeral state behave normally within one
daemon lifetime. The tradeoff is that invalid durable state blocks replay and
requires explicit recovery.

Store ownership and overlays are described by [ARCH-tau-core](ARCH-tau-core.md).
Persistence and replay behavior is specified by
[SPEC-tau-harness-session-state](../../tau-harness/specs/SPEC-tau-harness-session-state.md).
Changes remain governed by
[DECISION-persistence-and-extension-interface-change-approval](../../../specs/DECISION-persistence-and-extension-interface-change-approval.md).
