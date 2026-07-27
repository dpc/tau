# DECISION-agent-initialization-discovery-snapshots: Atomic agent initialization discovery

Authority: confirmed, 2026-07-26, dpc

## Decision

Tau uses complete atomic snapshots, rather than positive item-at-a-time
announcements, for extension-contributed skills and AGENTS.md discovery.
Omission removes a previous contribution, and an empty snapshot clears the
source.

Each agent initialization receives and freezes a separate effective snapshot
after all captured context providers settle. A later refresh, disconnect, or
another agent's initialization cannot alter a finalized agent snapshot.

Before releasing the first prompt, the harness durably replaces that agent's
bootstrap context and effective skill snapshot. This state remains outside
ordinary compactable transcript history. Harness-authored transient projections
expose current accepted state to live and late subscribers.

Role availability and required-skill preflight remain determined by the
validated session baseline. An agent-specific refresh failure may narrow that
agent's truthful snapshot but does not introduce a second role-validity gate.

The old item-declaration events are removed rather than supported in parallel;
no persisted-data or protocol migration is provided.

## Rationale

Positive item announcements cannot express deletion, expose intermediate
collision winners, and repeatedly append growing instruction stacks. Complete
replacement makes refresh atomic, while a frozen per-agent snapshot keeps
model and UI surfaces consistent. A durable replacement slot makes the newest
bootstrap authoritative without accumulating stale transcript history.

This decision supersedes the item-declaration and append-only injection choices
in
[DECISION-session-discovery-declarations-and-readiness](DECISION-session-discovery-declarations-and-readiness.md).
It follows
[DECISION-event-log-first-extension-state](DECISION-event-log-first-extension-state.md),
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md),
and [DECISION-no-backward-compatibility](DECISION-no-backward-compatibility.md).
