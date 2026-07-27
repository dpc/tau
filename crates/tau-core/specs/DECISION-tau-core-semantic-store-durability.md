# DECISION-tau-core-semantic-store-durability: Semantic store durability modes

Authority: unconfirmed

## Decision

Semantic stores support journal-backed and memory-only streams. Memory-only
streams fold the same live facts and support same-daemon replay without
artifacts. A locked journal recovery keeps the longest framed, decoded,
sequence-valid, semantically valid prefix and truncates the first invalid frame
plus its entire suffix.

## Rationale

The shared semantic boundary lets ephemeral state behave normally within one
daemon lifetime. Prefix recovery preserves the only history that replay can
prove valid without salvaging plausible bytes after corruption.

Exact behavior is specified by
[SPEC-tau-harness-session-state](../../tau-harness/specs/SPEC-tau-harness-session-state.md).
Writeback and crash-cut semantics are governed by
[DECISION-semantic-journal-writeback-durability](../../../specs/DECISION-semantic-journal-writeback-durability.md),
which supersedes this record where older wording implies whole-journal rejection.
