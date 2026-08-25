# GATE-asynchronous-journal-durability: Keep journal filesystem I/O off runtime paths

## Gate

Normal live paths validate and perform required in-memory semantic ordering and
fold work, then use bounded nonblocking persistence admission. Queue saturation
rejects before canonical acceptance. Ordered persistence workers perform
journal and journal-derived checkpoint filesystem writes using failure-atomic
techniques. Canonical publication does not wait for write or synchronization
completion.

Later storage failure does not retract an accepted event; diagnostics and retry
remain asynchronous. A process, kernel, or power crash may lose recent admitted
events. Tau must not introduce per-event durability acknowledgements,
sync-before-action ordering, or write-ahead-log-style commit gating.

Worker-side persistence preserves complete ordered frames, partial-write
rollback and poisoning, atomic checkpoint replacement and recovery, journal
order, and bounded memory and backpressure. Observational events added for
tracing or correlation remain best-effort and never make tool dispatch, wait
behavior, activation delivery, cancellation, provider continuation, or another
runtime outcome wait for filesystem I/O or change because storage later fails.

## Justification

The user deliberately chose low interactive latency over crash-complete
durability of the newest journal entries. Filesystem writes and synchronization
can have significant tail latency, so failure-atomic I/O belongs on bounded
workers rather than canonical live paths.
