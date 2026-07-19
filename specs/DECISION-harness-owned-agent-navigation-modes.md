# DECISION-harness-owned-agent-navigation-modes: Shared runtime navigation classification

Authority: confirmed, 2026-07-18, dpc

The harness owns one `AgentNavigationMode` (`active`, `active_auto`, or
`suspended`) for each currently loaded agent in its current session. This is
daemon-lifetime current state rather than a durable fact or preference.

`active` is always navigation-eligible, `active_auto` is eligible exactly while
the harness-authored runtime snapshot is `running`, and `suspended` is not
eligible. Modes do not change loading, routing, delivery, watches, execution, or
model behavior.

User-created agents default to `active`; extension/delegation-created agents
default to `active_auto`. UIs request absolute mode changes, while transient
harness-authored snapshots are the sole UI cache authority. This centralizes
eligibility across clients at the cost of intentionally forgetting mode on cold
restore. Exact behavior is specified by
[SPEC-tau-proto-session-events](../crates/tau-proto/specs/SPEC-tau-proto-session-events.md).
