# GATE-exact-event-subscriptions: Exact event subscriptions by default

## Gate

Protocol subscribers must list the concrete event names they handle.
Whole-category prefixes such as `agent.*`, `tool.*`, and `provider.*` are
reserved for intentionally generic observers.

## Justification

The user wants new event types to require deliberate subscriber adoption rather
than silently expanding traffic, replay, prompt exposure, or side effects.
