# ARCH-tau-supervisor: tau-supervisor architecture

`tau-supervisor` owns child process launch and stdio transport primitives used to prototype supervision behavior outside the production harness path.

## Scope

This crate is non-production. The production extension supervisor remains in
`tau-harness`; guarantees here apply only to direct users until the harness
integrates this crate or independently adopts its contracts.

## Process ownership

`SupervisedChild` owns one direct child. Failed initialization kills and waits
for a child that was already spawned. After successful construction, one waiter
thread owns the `std::process::Child`; observers consume its single exit
notification rather than polling child status.

Linux termination uses a pidfd opened before initialization ownership transfers,
avoiding numeric-PID reuse after the waiter reaps the child. Hard termination is
unsupported on other targets under this design. Callers should prefer protocol
shutdown or explicit `terminate`; `Drop` is best effort. Termination covers only
the direct child, not its process tree.

## Stdio transport

Children exchange CBOR protocol frames over stdin/stdout. A reader thread
decodes stdout frames and forwards them through a bounded queue; because decode
precedes queueing, that queue does not bound one oversized encoded frame.
Callers must continue draining a child that can emit during shutdown instead of
waiting indefinitely for exit.

## Child environment

Children inherit the supervisor environment except names starting
`TAU_SECRET_`. With an explicit working directory, the program path must be
absolute so executable resolution does not depend on platform-specific command
semantics.

`tau-supervisor` launches trusted local child programs. It does not sandbox child code, validate child behavior, or protect the host from a malicious configured executable. Do not pass secrets through environment variable names outside the stripped `TAU_SECRET_` namespace unless the child is trusted to receive them.
