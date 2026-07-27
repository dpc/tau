# DECISION-tau-core-semantic-store-durability: Semantic store durability modes

Authority: unconfirmed

## Decision

Semantic stores support durable and memory-only streams. Memory-only streams
fold the same live facts and support same-daemon replay without durable
artifacts. Durable replay fails closed on an invalid journal rather than
skipping or partially folding it.

## Rationale

The shared semantic boundary lets ephemeral state behave normally within one
daemon lifetime. Fail-closed replay prevents corrupted durable state from being
mistaken for a valid partial history.

Exact behavior is specified by
[SPEC-tau-harness-session-state](../../tau-harness/specs/SPEC-tau-harness-session-state.md).
