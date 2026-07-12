# SPEC-tau-ext-shell-process-lifecycle: Shell process lifecycle

Shell execution keeps foreground child ownership in the coordinator that reports
the tool or user-command result. Watcher threads may report process exit,
cancellation, deadlines, pipe readiness, or bounded pipe drain completion, but
they must not own or reap the foreground `Child`; the coordinator remains
responsible for `kill()` / `wait()` on cancellation and timeout.

On Unix, commands run in a separate session/process group and cancellation or
timeout kills that process group. Unix pipe readers are nonblocking and shell
completion does not depend on stdout/stderr EOF, because escaped descendants can
keep pipe file descriptors open after the foreground shell exits. On
non-Unix/Windows, cancellation and timeout use direct-child termination and a
bounded post-terminal pipe drain; descendants outside the direct child may still
survive, but they must not make the shell result wait indefinitely for pipe EOF.
After the bounded drain, a terminal flag prevents non-Unix pipe readers from
mutating final captures or publishing user-shell progress; a reader already
blocked in the OS may remain dormant until the descendant-held pipe wakes or
closes.

User-facing `!` / `!!` shell commands stream bounded progress events separately from
their bounded final captured output. After the per-stream progress cap is hit, ext-shell
keeps draining child pipes for process liveness and final truncation metadata but stops
forwarding arbitrary output volume into the event stream.

Shell lifecycle waits should use event/readiness channels or platform wait
primitives where available. Fixed sleep polling of child exit, cancellation, or
deadlines is not intended for supported lifecycle paths.
