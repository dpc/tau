# DECISION-exact-event-subscriptions: Exact event subscriptions by default

Authority: confirmed, 2026-07-16, dpc

Tau protocol subscribers list the concrete event-name selectors they actually
handle. Whole-category prefixes such as `agent.*`, `tool.*`, or `provider.*` are
reserved for subscribers intentionally acting as generic observers of that
category.

Exact subscriptions keep new event types from silently expanding existing
traffic, replay catch-up, prompt exposure, or side effects. First-party extensions
and UIs update their selector lists deliberately when their handlers learn a new
event. Historical and live sets may differ when a consumer intentionally handles
an event only in one path; that exception belongs with the consumer's behavior and
tests rather than broadening its subscription.
