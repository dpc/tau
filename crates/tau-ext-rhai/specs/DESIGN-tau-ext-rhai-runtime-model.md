# DESIGN-tau-ext-rhai-runtime-model: Single-threaded script runtime with supervised shell workers

Status: unconfirmed

The Rhai interpreter runs on one main runtime thread, while tau-client-owned
harness reading/writing and crate-owned shell execution use helper threads.
Shell workers are owned by runtime state through cancellation/process-group
handles and join handles, so disconnect and runtime drop synchronously cancel,
kill, and reap pending shell work before `run` returns, subject to a bounded
shutdown join timeout.

This keeps script execution non-concurrent while still allowing host shell
commands to run without blocking harness frame handling.

The main runtime loop is event-driven. It must not use fixed polling intervals
to discover shell completions or harness input; shell workers and the tau-client
protocol reader wake the loop after enqueueing work. The loop must drain ready
sources fairly so a flood of harness events cannot indefinitely delay a shell
completion callback.

Shell supervision follows the same rule below the runtime loop: worker shutdown
joins, child-process exit/timeout/cancellation, and Unix pipe capture are driven
by completion notifications, channels, or OS readiness. Do not reintroduce
`JoinHandle::is_finished`, `Child::try_wait`, or sleep-based readiness polling
for these paths.

Shell output capture is also bounded after foreground completion, timeout, or
cancellation. Unix commands run in an owned process group/session, but detached
descendants can survive with inherited stdout/stderr pipes; the runtime drains
only immediately available pipe output for a bounded post-stop window instead of
waiting for pipe EOF.
