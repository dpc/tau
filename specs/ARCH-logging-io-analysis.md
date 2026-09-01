# ARCH-logging-io-analysis: Logging sink I/O analysis boundary

Once application state has emitted a logging or `tracing` record to its sink,
Tau analyzes that sink I/O as a zero-time operation. Sink latency is not a
functional protocol, lifecycle, publication, or latency violation; deployments
where it matters may interpose a buffer, sponge, or equivalent transport.

This analysis rule applies only after emission to a logging or tracing sink. It
does not exempt protocol or persistence I/O, provider or network operations,
supervised extension stderr acquisition and draining, authoritative raw-log
writes required for functional progress, or any other application-level I/O.
It complements
[GATE-nonblocking-event-log-filesystem-io](GATE-nonblocking-event-log-filesystem-io.md)
without weakening that gate's canonical publication and persistence-queue
guarantees.
