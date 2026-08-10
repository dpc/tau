# GATE-nonblocking-event-log-filesystem-io: Keep event-log filesystem I/O off live publication paths

## Gate

During normal live publication, canonical acceptance, ordering, and admission
to the process-local live delivery stream may wait only for immediate bounded
in-memory persistence-queue admission, never filesystem I/O or its completion.
Persistence admission must be nonblocking; when full, it must reject before
canonical acceptance rather than block, discard an accepted event, or grow
without bound. Live delivery admission does not imply pipe or socket completion
or an extension acknowledgement.

One ordered persistence worker must preserve canonical journal disk order, but
frame completion need not precede broadcast. An admitted and published event
remains accepted when storage later fails; diagnostics and retry handle that
failure asynchronously without retraction. This includes filesystem
initialization or recovery lazily triggered by live publication, direct runtime
semantic journal appends such as `append_best_effort_observation` even when not
broadcast, and harness-owned debug or file mirrors and future harness-owned
file tracing. Externally installed tracing subscribers are outside this
guarantee.

This gate excludes startup, explicit recovery or replay, shutdown, retention,
maintenance or compaction, export or snapshot work, extension-data RPCs, and
skill reads.

## Justification

The user wants slow or stalled storage never to delay canonical live
publication or admission to the logical live delivery stream. Bounded
asynchronous persistence keeps the live harness responsive without hiding
queue pressure or retracting already accepted events.
