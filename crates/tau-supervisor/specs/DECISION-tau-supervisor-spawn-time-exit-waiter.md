# DECISION-tau-supervisor-spawn-time-exit-waiter: Spawn-time child exit waiter

Authority: unconfirmed

`SupervisedChild` transfers its `Child` handle to a waiter at spawn time and receives
one reactive exit notification instead of polling. On Linux, hard termination uses
a pidfd opened before handoff; other targets have no unsafe numeric-PID fallback and
must rely on protocol shutdown.

This accepts a platform limitation to avoid PID-reuse races and sleep-based exit
latency. Ownership and API behavior are documented in
[ARCH-tau-supervisor](ARCH-tau-supervisor.md).
