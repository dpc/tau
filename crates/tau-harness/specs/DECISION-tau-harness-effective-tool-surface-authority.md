# DECISION-tau-harness-effective-tool-surface-authority: Harness-owned effective tool surface

Authority: unconfirmed

## Decision

For each provider prompt, the harness owns one immutable effective tool snapshot
after role policy and provider capability filtering. That snapshot is the single
authority for provider definitions, capability claims, authorization, and
prompt-owned rejection diagnostics. Later mutable state cannot rewrite it.

Extensions and providers publish neutral metadata rather than choosing which
model receives which tool surface. Effective visible-name collisions are
rejected instead of resolved by registry order.

One harness-owned snapshot prevents advertised capabilities, authorization, and
diagnostics from disagreeing across mutable runtime boundaries. The tradeoff is
explicit dispatch failure when no coherent snapshot can be built.

Exact filtering, policy precedence, sparse template data, parallel-call claims,
diagnostics, and lifecycle behavior are specified by
[SPEC-tau-harness-prompt-dispatch](SPEC-tau-harness-prompt-dispatch.md).
