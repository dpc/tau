# DESIGN-tau-supervisor-spawn-time-exit-waiter: Spawn-time child exit waiter

Status: unconfirmed

`SupervisedChild` starts a dedicated waiter thread during spawn. The waiter owns
the `std::process::Child` handle and sends the one child exit status over a
channel; `try_wait` and `wait_for_exit` observe and cache that notification. This
keeps timed exit waits reactive without a supervisor-side `try_wait`/sleep loop.

Because the waiter owns the `Child`, Linux hard termination uses a pidfd opened
before the spawn cleanup guard is disarmed. The pidfd preserves direct-child
targeting without a numeric PID reuse race. Non-Linux hard termination is
explicitly unsupported until there is an equivalent race-free process handle in
this crate; callers on those targets must rely on protocol shutdown.
