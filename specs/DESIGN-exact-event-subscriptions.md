# DESIGN-exact-event-subscriptions: Event subscribers list concrete events by default

Status: confirmed, 2026-07-03, user

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
