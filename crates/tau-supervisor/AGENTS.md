# tau-supervisor agent notes

Before modifying `crates/tau-supervisor`, read `ARCHITECTURE.md`,
`SECURITY.md`, and `design.md` in this directory. Preserve the documented
process-ownership, stdio transport, lifecycle pid, child environment, direct-child
cleanup, and integration-test fixture contracts.

Child exit waiting is intentionally reactive: `SupervisedChild` starts a
spawn-time waiter thread and must not reintroduce sleep-based child-exit polling.
Linux hard termination depends on the pidfd opened before waiter handoff; do not
replace it with raw numeric-pid signaling.
