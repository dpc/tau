# tau-supervisor security notes

`tau-supervisor` launches trusted local child programs. It does not sandbox child code, validate child behavior, or protect the host from a malicious configured executable.

## Environment

Children inherit the supervisor process environment except variables whose names start with `TAU_SECRET_`; those are stripped before launch. Do not pass secrets through other environment variable names unless the child is trusted to receive them.

## Process cleanup

Cleanup targets only the direct child process owned by `SupervisedChild`. The waiter thread owns the `Child` handle after spawn; on Linux, `SupervisedChild` keeps a pidfd opened before that handoff and uses it for hard termination so cleanup cannot hit an unrelated reused PID. `terminate` and `Drop` do not kill a process tree or any grandchildren that the child may leave behind. Hard termination is currently Linux-only; non-Linux callers should use protocol shutdown because there is no race-free hard-kill handle in this implementation.

## Stdio transport

The stdout reader uses a bounded handoff queue after each frame is decoded. Individual protocol frames are decoded before queueing, so this does not bound memory used to decode one oversized frame. Callers must keep draining stdout for children that may emit during shutdown, otherwise backpressure can block the child.

## Shutdown expectations

Callers should prefer explicit protocol shutdown when possible. On Linux, `terminate` is hard-kill direct-child cleanup with an observable result; `Drop` is last-resort best-effort cleanup with ignored errors.
