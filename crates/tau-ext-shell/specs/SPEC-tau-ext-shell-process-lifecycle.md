# SPEC-tau-ext-shell-process-lifecycle: Shell process lifecycle

## Record justification

The contract spans shared spawning, model and user shell coordinators, platform
wait implementations, cancellation, and bounded output capture, so no listed
implementation area coherently owns it whole.

On Unix, shell execution transfers the foreground `Child` to one dedicated
waiter, which reaps it and reports process exit. The result coordinator retains
the process-group identity, handles cancellation and deadlines, initiates group
termination, drains bounded output, and reports the result. Non-Unix
coordinators retain and reap the child directly.

On Linux, Android, and macOS, commands attach stdout and stderr to independent PTYs.
This retains stream identity while making both output descriptors TTY-like;
stdin remains closed and persistently ready at EOF. PTY readers are nonblocking
and shell completion does not depend on stdout/stderr EOF because escaped
descendants can keep user endpoints open after the foreground shell exits.
Other Unix targets, including unvalidated BSD targets, retain pipe capture. The
supported allocators create Tau's PTY endpoints atomically close-on-exec; this
guarantee does not cover independent descriptors allocated inside the
platform's process-spawn implementation. On all Unix targets,
commands run in a separate session/process group and cancellation or timeout
kills that process group. On
non-Unix/Windows, cancellation and timeout use direct-child termination and a
bounded post-terminal pipe drain; descendants outside the direct child may still
survive, but they must not make the shell result wait indefinitely for pipe EOF.
Model shell calls carry cancellation state across scheduling and automatic-lock
handoffs. Cancellation before the shared effect-start transition prevents spawn
and reports one cancelled terminal; once effect start wins, cancellation retains
the platform behavior above and does not promise rollback.
Before spawning either a model or user shell command, the shared boundary
normally applies the protected non-interactive pager environment.
After the bounded drain, a terminal flag prevents non-Unix pipe readers from
mutating final captures or publishing user-shell progress; a reader already
blocked in the OS may remain dormant until the descendant-held pipe wakes or
closes.

User-facing `!instance command` / `!!instance command` shell commands stream
bounded opaque endpoint progress replies separately from
their bounded final captured output. After the per-stream progress cap is hit, ext-shell
keeps draining child output for process liveness and final truncation metadata but stops
forwarding arbitrary output volume into the event stream.
The extension owns acceptance, duplicate rejection, and cancellation, including
sticky cancellation while queued. A request targets its configured name and
process nonce; restart never retries execution. After `!` finishes, the extension
requests generic passive context insertion; `!!` does not.
See [SPEC-extension-owned-shell-interaction](../../../specs/SPEC-extension-owned-shell-interaction.md).

Model shell capture retains complete byte and line totals while bounding memory.
Its native `output` body is capped separately from the larger ephemeral saved
rendering. Small result metadata and fully rendered provider text are outside
that body budget, and hitting the saved-rendering cap must be reported as an
incomplete artifact rather than as full output.
Completed model commands also expose the spool metadata when the harness's
smaller final-rendering allowance omits bytes or earlier live batches leave
only a terminal tail. The temporary file remains ordinary shell-instance
storage, not a shared artifact. Cancellation keeps its existing available-tail
contract without starting a new saved-output spool.

Shell lifecycle waits should use event/readiness channels or platform wait
primitives where available. Fixed sleep polling of child exit, cancellation, or
deadlines is not intended for supported lifecycle paths.
