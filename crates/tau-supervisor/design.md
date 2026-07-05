# Design decisions

This file records major design decisions currently embodied by this directory's
code, and how authoritative each decision is. It is not an architecture
overview, ADR log, todo list, roadmap, implementation guide, or changelog.

## Integration-test child fixture

Status: inferred

`tau-supervisor` verifies process lifecycle, stdio framing, environment
filtering, stderr policy, and direct-child cleanup with integration tests that
spawn a real local child process.

The fixture lives at `src/bin/tau-supervisor-test-child.rs` intentionally. Cargo
then builds it as a normal binary target and exposes its path to integration
tests through `CARGO_BIN_EXE_tau-supervisor-test-child`, avoiding test-only path
guessing and ensuring the fixture exercises the same binary-launch mechanics as
supervised children. The binary is an internal test fixture, not a production
entrypoint.

Keep fixture behavior narrow and deterministic. Add new fixture modes only for
contracts that require a real subprocess boundary and keep those modes coupled to
`tests/supervisor.rs`.

## Spawn-time child exit waiter

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
