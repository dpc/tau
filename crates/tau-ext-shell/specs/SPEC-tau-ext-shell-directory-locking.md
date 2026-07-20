# SPEC-tau-ext-shell-directory-locking: Directory locking

`tau-ext-shell` owns advisory directory update coordination for its tools. The
agent-visible tool is named `dir_lock` because Tau tool names cannot contain
hyphens. The harness does not provide update/exclusive scheduling for tools or
agent creation, and protocol tool and agent-start messages carry no such scheduling
metadata. A sub-agent is an independent lock owner; its parent's manual locks do not
cover it.

## Configuration and cwd

Directory locking is opt-in. The `dir_lock` tool is registered disabled by default,
and `dir_lock.enable = true` enables its handler and automatic locking for mutating
ext-shell tools. When locking is disabled, `shell` / `gpt_shell` calls are ordinary
read-write commands and no access-mode chip is published for UI display. User
`!` shell commands are UI commands rather than agent tool calls and do not
participate in locking.

The top-level `workdir(path)` setter changes the remembered workdir by emitting
`agent.metadata_set` and completing only after its committed echo. Explicit
generic `shell.cwd` and ChatGPT-facing `shell_command.workdir` arguments are
call-local and never update metadata.
Relative paths for filesystem tools
(`read`, `edit`, `find`, `grep`, `ls`, `apply_patch`, and `dir_lock`) are resolved
against the admission-time remembered workdir before execution or automatic lock selection. Once automatic
lock selection begins, the invocation carries the same cwd snapshot through lock waiting
and execution, even if committed cwd metadata changes before the lock is granted. This
keeps locks, shell execution, and patch paths aligned without calling `chdir(2)` in the
extension process.

The default directory-lock backend is `memory`, which coordinates only workers inside
one ext-shell process. The backend is initialized only when directory locking is
enabled, so selecting `filesystem` while locking remains disabled does not create or
validate filesystem state. The optional `filesystem` backend stores manual locks,
automatic locks, and arrival-ordered waiters in a JSON registry protected by `fs2`
file locks. It uses `dir_lock.state_dir` when configured, otherwise
`$XDG_RUNTIME_DIR/tau/ext-shell-dir-locks`, and otherwise a verified private temporary
fallback. Its registry directory must be private (`0700` on Unix); unsafe
initialization fails closed instead of silently using process-local locking. Each
ext-shell instance holds an exclusive lease lock under the registry's `instances/`
directory; other instances reap registry records whose lease locks are no longer held.
The filesystem backend therefore coordinates Tau/ext-shell processes on the same host
and user account without treating timestamps as liveness proof. Filesystem
`instance_id`s are internal lease identifiers; model/user-visible diagnostics and
`owner_agent_id` recovery use only `AgentId`.

Backend reconfiguration initializes the requested backend before swapping it in, so
initialization failure is reported as `ConfigError` while the previous backend and its
lock state remain active. Backend swaps are rejected while automatic locks are active,
because those guards release through the backend that granted them and must remain
visible to later acquisitions. Automatic guards retain the lease and release handle
that granted them through backend reconfiguration.

## Manual locks and scheduling

`dir_lock` accepts a `command` of `update` or `unlock`, an existing `directory`,
and an optional `owner_agent_id` that is meaningful only for `unlock`. Directory
arguments are canonicalized; missing paths and non-directories are errors.

`update` acquires a manual lock for the canonical directory and calling `agent_id`.
`unlock` releases a matching manual lock held by the caller, or by `owner_agent_id`
when supplied. A second manual `update` from the same owner for the same directory,
an ancestor, or a descendant fails as `error: dir_lock_duplicate`; its structured
details contain `blocking_directory`, `requested_directory`, and `lock_owner_id`,
with a short explanation in `output`.

Locks conflict when either canonical directory contains the other. Reads do not
participate. Granting is path-local FIFO in both backends: an active lock blocks
overlapping requests, while an earlier queued waiter blocks only later waiters whose
requested directories overlap. Unrelated later waiters may proceed. Queued same-owner
manual waiters revalidate duplicate-lock rules before grant. Same-owner automatic
reentry is allowed so tools can mutate under their owner's manual lock without
deadlocking; repeated manual acquisition remains an error.

Manual locks track acquisition time, last-use time, and active automatic tools under
the lock. A waiter blocked by an active manual lock, but not by an earlier overlapping
waiter, checks liveness every 60 seconds. Once that lock has been idle for 120 seconds
with no active automatic tool, the waiter fails as `error: dir_lock_abandoned`.
Structured details contain `blocking_directory`, `lock_owner_id`, `idle_seconds`, and
`held_seconds`, with recovery guidance in `output`.

## Automatic locking

When directory locking is enabled, mutating ext-shell tools acquire automatic locks
before execution:

- `edit` locks the target file's parent. Existing final symlinks resolve to the real
  edited file. When parents are missing, it locks the deepest existing ancestor.
- `apply_patch` parses the patch and locks all touched source and destination
  directories as one FIFO request.
- `shell` and `gpt_shell` infer access mode rather than accepting an explicit mode.
  A command whose canonical call-local `cwd` (generic `shell`) or `workdir`
  (`shell_command`), or the agent's remembered workdir when that argument is
  omitted, is covered by the caller's manual lock is read-write and takes
  an automatic lock; otherwise it is read-only and skips update locking.

Automatic locks last for the invocation and serialize with manual locks and other
automatic mutating calls. Automatic calls under a covering same-owner manual lock
reenter the writer section and do not wait on one another; other owners remain
blocked until the manual lock is released and all active automatic calls finish.
Calls enter ext-shell's bounded scheduler before lock acquisition, so a blocked
mutating call occupies a worker until granted, cancelled, diagnosed as abandoned,
or cancelled during session cleanup.

The read-write inference and automatic lock acquisition happen under the
`DirLockManager` state lock. A shell call queued as read-write must still have covering
manual-lock ownership at the moment the automatic lock is granted; otherwise it falls
back to read-only execution instead of running under stale coverage.

Filesystem-backend waiters keep the process-local condition-variable wake path used by
the memory backend for same-process releases and cancellation. These notifications are
paired with a process-local wake generation under the `DirLockManager` state mutex, so a
notification that arrives between a registry check and the timed wait cannot be lost.
For cross-process registry changes, where there is no portable peer wake primitive,
waiters use adaptive timed re-checks starting at 50 ms and doubling to a 1 s ceiling.
The separate abandoned-lock liveness deadline caps actual sleeps; timed sleeps consume
backoff, while same-process condition-variable notifications reset it.

`read`, `grep`, `find`, `ls`, and inferred read-only shell calls remain runnable while
update locks are held.

## Cleanup, recovery, and UI

Manual locks are released when ext-shell observes `agent.start_result` for a tracked
delegate or side agent, `SessionAgentUnloaded` for the owner, or session termination
through shutdown, disconnect, or EOF. Session cleanup also cancels queued waiters so
worker shutdown can complete promptly.

The `/shell-dir-force-unlock DIRECTORY` UI action canonicalizes an existing directory
and releases all overlapping manual locks, regardless of owner. It does not cancel or
release automatic locks held by running tools. An ancestor or descendant displayed by
a waiting call can therefore be used to clear the conflicting manual lock.

Blocked ext-shell calls submit `tool.progress_reported` with a live
`ToolDisplay` naming the
directory or directories being awaited. Terminal `dir_lock` success and failure
displays include the relevant directory when known. Normal foreground and
auto-background behavior still applies because the harness sees the tool call as
running until ext-shell submits a terminal `tool.*_reported` event.

## Security and precision boundaries

`tau-ext-shell` executes local commands and mutates local files with the user's
permissions. Directory update locks are advisory coordination for Tau/ext-shell tools,
not an operating-system sandbox or access-control boundary.

Directory-lock leases end when ext-shell exits. A detached shell descendant can
therefore continue mutating files after the lease is released and another Tau instance
proceeds.

Shell update locking is also advisory: a read-write command can mutate paths outside
its cwd through absolute paths or command-specific flags. Inferred read-only shell mode
is defense in depth. With directory locking enabled, `dir_lock.enforce_ro_bind` defaults
to `true` and requires a native read-only bind mount of the command cwd; unsupported or
failed isolation makes the shell call fail closed. If a user explicitly sets it to
`false`, inferred read-only execution degrades to ordinary command execution and is not
a hard sandbox or operating-system access-control boundary.

Creates beneath missing parents use the deepest existing ancestor and are therefore
safe but less precise. Same-owner reentry can keep other agents waiting as long as the
owner retains a manual lock; this is intentional manual-lock behavior rather than FIFO
starvation. Mutating tools outside ext-shell receive no harness-level update/exclusive
serialization and must arrange their own coordination.
