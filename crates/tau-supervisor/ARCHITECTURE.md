# tau-supervisor architecture

`tau-supervisor` owns child process launch and stdio transport primitives used to prototype supervision behavior outside the production harness path.

## Scope

This crate is currently non-production. The production harness supervisor still lives in `tau-harness`, so reliability guarantees here only apply to direct users of this crate until the harness integrates it or duplicates the same contracts.

## Process ownership

`SupervisedChild::spawn` owns the spawned direct child from successful process creation onward. During initialization a guard kill/waits the child if pipe setup or stdout-reader startup fails. Once construction succeeds, callers should prefer explicit protocol shutdown or `SupervisedChild::terminate`; `Drop` is only best-effort hard-kill cleanup with ignored errors. Termination intentionally targets only the direct child process, not a process tree or grandchildren.

Lifecycle helpers on `SupervisedChild` derive process pids from the owned child. Use `ExtensionCommand::pre_spawn_starting_event` only for pid-less pre-spawn lifecycle reporting.

## Stdio transport

Children communicate with the harness over CBOR protocol frames on stdin/stdout. A dedicated stdout reader thread decodes frames and forwards them through a bounded buffer. Callers supervising a child that may emit during shutdown must keep draining stdout or avoid waiting indefinitely for exit.

Receive outcomes distinguish decoded messages, timeout, and clean stdout closure. Corrupt or truncated frames remain decode errors.

## Integration-test fixture

Integration tests use the `tau-supervisor-test-child` binary target in
`src/bin/` as a real subprocess fixture. Keeping it as a Cargo binary target lets
tests use `CARGO_BIN_EXE_tau-supervisor-test-child` and exercise normal
child-launch semantics. The fixture is internal to this crate's tests and is not
part of the production supervisor path.

## Child environment

Children inherit the supervisor environment except variables with names starting `TAU_SECRET_`; those are stripped before launch. Command configuration also controls argv, optional working directory, and stderr policy. When `working_dir` is set, `program` must be absolute; relative program paths are rejected so executable resolution does not depend on platform-specific `Command` semantics.
