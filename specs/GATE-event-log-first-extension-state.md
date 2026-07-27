# GATE-event-log-first-extension-state: Event-log-first durable extension state

## Gate

When committed Tau facts can completely represent the non-secret inputs needed
to restore extension-owned state, the ordered Tau journal must remain its sole
durable source of truth; extensions must not add a parallel durable snapshot or
unbounded shadow set.

## Justification

The user wants one authority for ordering, commit, replay, and recovery instead
of cross-store reconciliation. Separate private persistence remains appropriate
for secrets, large or subscriber-unsafe data, restart-guaranteed remote intent,
state that outlives retained history, or demonstrated replay constraints.
