# DESIGN-exact-event-subscriptions: Event subscribers list concrete events by default

Status: confirmed, 2026-07-16, dpc

Tau protocol subscriptions should use exact event-name selectors for the events
the subscriber actually handles. Whole-category prefix subscriptions such as
`agent.*`, `tool.*`, or `provider.*` are reserved for cases where the subscriber
is intentionally a generic observer for that category and the broader traffic is
part of its design.

This keeps new event types from automatically expanding existing subscribers'
traffic, replay catch-up, prompt-surface exposure, or side-effect triggers.
First-party extensions and UIs that only react to a known subset of events should
therefore spell that subset out explicitly and update it deliberately when their
handlers learn a new event.

Historical and live selector sets may intentionally differ. In particular, the
chat UI receives `tool.request` and `tool.started` live but does not request them
from the append-only restore-event history: replaying a completed call's old
start would transiently resurrect it as pending before its durable terminal
agent result arrives. Generic observers and extensions may still request those
replay-marked execution facts when their design requires them.
