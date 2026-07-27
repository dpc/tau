# GATE-asynchronous-journal-durability: Keep journal durability off runtime paths

## Gate

Tau must not wait for journal file-data or directory synchronization before
continuing runtime work after a semantic append. Journal writeback must remain
asynchronous and coalesced; a system crash may lose the unsynchronized suffix
of otherwise accepted events.

Observational events added for tracing or correlation must remain best-effort.
Tool dispatch, wait behavior, activation delivery, cancellation, provider
continuation, and other runtime outcomes must not wait for those observations
to reach durable storage or change because their logging fails. Tau must not
introduce per-event durability acknowledgements, sync-before-action ordering,
or write-ahead-log-style commit gating for these effects.

This constraint does not remove immediate append validation, failure-atomic
frame writes, event ordering, existing semantic-event append requirements, or
replay semantics. It separates those concerns from stable-storage durability
and prevents new observational logging from becoming a runtime precondition.

## Justification

The user deliberately chose low hot-path latency over crash-complete durability
of the newest journal entries. A session journal is an event log, not a
transactional database. Consumers must tolerate an incomplete crash tail rather
than move storage synchronization into runtime paths.
