# GATE-atomic-event-log-publication: Preserve atomic event-log publication

## Gate

The event log publishes atomic semantic transition events for asynchronous
reaction by other components. Higher-level operations with dependent phases must
be explicit multi-step state machines that advance through ordinary
committed-event and publication outcomes. They must publish the semantic
transitions those state machines need, without spamming the log with
transient/internal bookkeeping or unnecessarily duplicated or large payloads;
infrequent operations may use reasonable multiple small events when correctness
or clarity requires them. Event-count or payload optimization must not justify
pseudo-transactions, multi-event atomic bundles, semantic reservations, or
pre-persist, suppression, or reconstruction mechanisms that make dependent
semantic events behave as one event-log operation.

## Justification

The user wants the event log to remain a simple atomic broadcast and
publication primitive, rather than a dumping ground or a mechanism for hiding
dependent semantic work as one operation. Explicit state-machine transitions
keep those operations independently observable and asynchronously handled by
their consumers.

This gate does not concern event vocabulary or payload evolution. Ordinary
backpressure or capacity reservation, and implementation of persistence for one
event, remain appropriate when they preserve this atomic publication model.
