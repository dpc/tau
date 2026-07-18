# DECISION-harness-owned-agent-navigation-modes: Shared runtime navigation classification

Authority: confirmed, 2026-07-18, dpc

The harness owns one `AgentNavigationMode` (`active`, `active_auto`, or
`suspended`) for each currently loaded agent in its current session. This is
daemon-lifetime current state, not a semantic session/agent fact, checkpoint, or
durable preference. Same-daemon reconnects share it; unload, session switch, and
process exit forget it. Cold restore recomputes the existing defaults.

`active` is always navigation-eligible, `active_auto` is eligible exactly while
the harness-authored runtime snapshot is `running`, and `suspended` is not
eligible. Modes do not change loading, routing, delivery, watches, execution, or
model behavior. UIs retain their own selected agent, visible transcript, drafts,
editor state, and presentation.

User-created agents default to `active`. Extension/delegation-created agents
default to `active_auto`; restore derives the same existing default from
replayable prompt provenance. A UI may request the absolute mutations
`set_active`, `set_active_auto`, and `set_suspended`. The harness serializes
accepted requests on its event loop, so later accepted writes supersede earlier
writes.

The mode is required in the transient complete `agent.stats_updated`
operational snapshot. These harness-authored snapshots are must-pass and
immutable. Live UIs receive a refreshed snapshot after every accepted mutation,
and catch-up reconstructs current snapshots before replay completion.
Requester-directed results are transient acknowledgements or diagnostics only;
snapshots alone update UI caches. Extensions cannot mutate this state.

No compatibility bridge or protocol-version bump is added under
[DECISION-no-backward-compatibility](DECISION-no-backward-compatibility.md).
This separately confirmed interface decision satisfies
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
The affected boundaries are described by
[the harness architecture](../crates/tau-harness/specs/ARCH-tau-harness.md),
[the protocol session-event specification](../crates/tau-proto/specs/SPEC-tau-proto-session-events.md),
and [the CLI slash-command specification](../crates/tau-cli/specs/SPEC-tau-cli-slash-commands.md).
Authority comes from the exact confirmation recorded in `tau-agent-al6q`.
