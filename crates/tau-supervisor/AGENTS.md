Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-supervisor agent notes

Before modifying `crates/tau-supervisor`, read `specs/ARCH-tau-supervisor.md`,
the applicable trust-boundary records under `specs/`, and the applicable Linked Specs under `specs/`. Preserve the documented
process-ownership, stdio transport, lifecycle pid, child environment, direct-child
cleanup, and integration-test fixture contracts.

Child exit waiting is intentionally reactive: `SupervisedChild` starts a
spawn-time waiter thread and must not reintroduce sleep-based child-exit polling.
Linux hard termination depends on the pidfd opened before waiter handoff; do not
replace it with raw numeric-pid signaling.
