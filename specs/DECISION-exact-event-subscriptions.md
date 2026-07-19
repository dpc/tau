# DECISION-exact-event-subscriptions: Exact event subscriptions by default

Authority: confirmed, 2026-07-16, dpc

Tau protocol subscribers list the concrete event-name selectors they actually
handle. Whole-category prefixes such as `agent.*`, `tool.*`, or `provider.*` are
reserved for intentionally generic observers.

Exact subscriptions keep new event types from silently expanding existing
traffic, replay catch-up, prompt exposure, or side effects. This costs explicit
subscription maintenance in exchange for preventing unreviewed authority and
traffic expansion.
