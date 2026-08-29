# ARCH-compaction-lifecycle: Compaction lifecycle

Compaction replaces a provider-valid closed prefix of one agent's transcript.
`AgentTree` is the sole durable semantic authority for that transcript; runtime
state is a disposable projection. Live admission and cold recovery fold the
same canonical facts to equivalent state.

Every request, start, and terminal correlation is scoped to its target agent.
At the documented commit boundaries, ownership moves from `AgentTree` history
to the harness transaction, to the selected provider adapter for
materialization and execution, and back to the harness for validation and
canonical replacement. Provider output remains private until that final
validation and commitment. Only the accepted replacement re-enters
`AgentTree` and may feed runtime projections.

[SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md)
owns the cross-component contract. [ARCH-tau-core](../crates/tau-core/specs/ARCH-tau-core.md)
owns durable folding, [ARCH-tau-harness](../crates/tau-harness/specs/ARCH-tau-harness.md)
owns transaction and publication boundaries, [ARCH-tau-provider](../crates/tau-provider/specs/ARCH-tau-provider.md)
owns provider-neutral materialization, [ARCH-tau-proto](../crates/tau-proto/specs/ARCH-tau-proto.md)
owns protocol representations, and [ARCH-tau-cli](../crates/tau-cli/specs/ARCH-tau-cli.md)
owns user-facing authority and projection.
