# SPEC-tau-ext-shell-directory-locking: Directory locking

The `cd` tool changes the remembered cwd by emitting `agent.metadata_set` and a
model-visible `agent.user_message_injected` notice. Explicit `cwd` arguments on shell
tools also emit metadata and update remembered cwd. Relative paths for filesystem tools
(`read`, `edit`, `find`, `grep`, `ls`, `apply_patch`, and `dir_lock`) are resolved
against the remembered cwd before execution or automatic lock selection. Once automatic
lock selection begins, the invocation carries the same cwd snapshot through lock waiting
and execution, even if committed cwd metadata changes before the lock is granted. This
keeps locks, shell execution, and patch paths aligned without calling `chdir(2)` in the
extension process.

Directory locking is opt-in. When disabled, `shell` / `gpt_shell` calls are ordinary
read-write commands and no access-mode chip is published for UI display. When enabled,
shell access mode is inferred from manual lock ownership: a command whose cwd is covered
by the caller's manual lock is read-write and takes an automatic lock; otherwise it is
read-only.

The default directory-lock backend is `memory`, which coordinates only workers inside
one ext-shell process. The optional `filesystem` backend stores the same manual locks,
automatic locks, and arrival-ordered waiters in a JSON registry protected by `fs2` file
locks. Its configured registry directory must be private (`0700` on Unix); unsafe
initialization fails closed instead of falling back to process-local locking. Granting
is path-local FIFO in both backends: an active lock blocks overlapping requests, and an
earlier queued waiter only blocks later queued waiters whose requested directories
overlap. Later unrelated waiters may proceed, while later overlapping waiters preserve
arrival order and queued same-owner manual waiters revalidate duplicate-lock rules
before grant. Each ext-shell instance holds an exclusive lease lock under the registry's
`instances/` directory; other instances reap registry records whose lease locks are no
longer held. Automatic guards retain the lease and release handle that granted them
through backend reconfiguration. The filesystem backend therefore coordinates
Tau/ext-shell processes on the same host and user account without treating timestamps as
liveness proof. Filesystem `instance_id`s are internal lease identifiers;
model/user-visible diagnostics and `owner_agent_id` recovery use only `AgentId`. The
backend is initialized only when directory locking is enabled. Backend reconfiguration
initializes the requested backend before swapping it in, so initialization failure is
reported as `ConfigError` while the previous backend and its lock state remain active.
Backend swaps are also rejected while automatic locks are active, because those guards
release through the backend that granted them and must remain visible to later
acquisitions.

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

`tau-ext-shell` executes local commands and mutates local files with the user's
permissions. Directory update locks are advisory coordination for Tau/ext-shell tools,
not an operating-system sandbox or access-control boundary.

Directory-lock leases end when ext-shell exits. A detached shell descendant can
therefore continue mutating files after the lease is released and another Tau instance
proceeds.

## Read-only shell isolation

Inferred read-only shell mode is defense in depth. With directory locking enabled,
`dir_lock.enforce_ro_bind` defaults to `true` and requires a native read-only bind mount
of the command cwd; unsupported or failed isolation makes the shell call fail closed. If
a user explicitly sets it to `false`, inferred read-only execution degrades to ordinary
command execution and is not a hard sandbox or operating-system access-control boundary.
