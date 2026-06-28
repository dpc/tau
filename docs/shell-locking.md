# Shell directory locking

This note documents the move of filesystem update coordination out of `agent_start` and harness scheduling and into `tau-ext-shell`.


## Current design

- `tau-ext-shell` owns directory update locking with an optional `dir_lock` tool.
- The tool name is `dir_lock`; Tau tool names do not allow hyphens.
- `dir_lock` is registered disabled by default. Setting ext-shell config `dir_lock.enable = true` enables the handler and opts mutating ext-shell tools into locking.
- `dir_lock.backend` defaults to `"memory"`, preserving historical process-local
  coordination. The backend setting is only initialized while
  `dir_lock.enable = true`; setting `backend = "filesystem"` with locking still
  disabled does not create or validate filesystem state. Setting
  `dir_lock.backend = "filesystem"` stores lock state in a
  per-user registry under `dir_lock.state_dir`, `$XDG_RUNTIME_DIR/tau/ext-shell-dir-locks`,
  or a verified private temp fallback so multiple Tau/ext-shell process instances
  on the same host and user account coordinate with each other.
- `agent_start` sub-agents are independent agents. A parent agent lock does not automatically cover a delegate.
- The harness no longer enforces tool or start-agent update/exclusive scheduling, and the protocol no longer carries scheduling metadata for tool specs, delegate progress, or start-agent requests.


## `dir_lock` semantics

Arguments:

- `command`: `update` or `unlock`
- `directory`: an existing directory
- `owner_agent_id`: optional owner to unlock; only meaningful with `command: unlock`

All `directory` values are canonicalized before use. Missing paths or non-directories are errors.

`update` acquires a manual update lock for the canonical directory and the owning `agent_id`. `unlock` releases one matching manual lock held by the calling agent, or by `owner_agent_id` when that optional argument is present. A second `update` by the same agent for the same directory, an ancestor, or a child is an error; manual double-locking is treated as a likely forgotten unlock. Duplicate-lock errors use `error: dir_lock_duplicate` with structured details headers: `blocking_directory`, `requested_directory`, and `lock_owner_id`, plus a short text payload in `output`.

Conflicts are based on path ancestry: a lock conflicts when either directory contains the other. Reads do not participate.

The wait queue is FIFO. If the front waiter is blocked, later waiters do not jump ahead. Same-owner automatic reentry is allowed so an agent holding a manual lock can keep using mutating tools under that lock without deadlocking itself. A repeated manual `dir_lock update` still errors.

Manual locks track acquisition time, last-use time, and active automatic tools running under the lock. A front FIFO waiter performs a liveness check every 60 seconds. If the blocking manual lock has been idle for 120 seconds and has no active automatic tools inside it, the waiter returns `error: dir_lock_abandoned` with structured details headers: `blocking_directory`, `lock_owner_id`, `idle_seconds`, and `held_seconds`, plus a short text payload in `output`.

Manual locks are released when ext-shell observes `agent.start_result` for a tracked delegate/side-agent, `SessionAgentUnloaded` for the owning agent, or session termination (`SessionShutdown`, disconnect, or EOF). Session-level cleanup also cancels queued lock waiters so worker shutdown can finish promptly. The extension also publishes a UI action `/shell-dir-force-unlock DIRECTORY` that canonicalizes an existing directory and force-releases all overlapping manual locks, regardless of owner. It does not cancel or release automatic locks held by currently running tools.


## Automatic locking for ext-shell tools

When `dir_lock.enable` is true, these mutating tools acquire automatic update
locks before running:

- `edit`: locks the target file parent. Existing final symlinks are followed to the real edited file. If parents are missing, it locks the deepest existing ancestor so line-oriented create/overwrite behavior remains intact.
- `apply_patch`: parses the patch and locks all touched source and destination directories as one FIFO request.
- `shell` and `gpt_shell`: no longer accept an explicit access-mode argument. If the calling agent holds a manual lock covering the canonical `cwd` (or the agent's remembered cwd when `cwd` is omitted), the shell call is inferred read-write and takes an automatic lock. Otherwise it is inferred read-only and skips automatic update locking. Relative `apply_patch`, `dir_lock`, and filesystem-tool paths are resolved against the same remembered cwd before lock selection/execution. Once lock selection starts, the tool executes with that cwd snapshot even if later cwd metadata commits while it is waiting.

Automatic locks are held only for the tool invocation duration. They serialize with manual locks and with other automatic mutating calls. When the calling agent already owns a covering manual lock, automatic calls under that lock reenter the same writer section and do not wait on same-owner automatic calls; other agents remain blocked until the manual lock is released and all active automatic calls finish. Tool calls are admitted to ext-shell's bounded scheduler before attempting automatic lock acquisition, so a blocked mutating call occupies a worker until the lock is granted, cancelled, abandoned, or session cleanup cancels waiters.

Backend reconfiguration fails with a configuration error while any automatic
lock is active. This keeps a running mutating tool protected by the backend that
admitted it until its guard releases, instead of switching later acquisitions to
a different backend that cannot see the active lock.

`read`, `grep`, `find`, `ls`, and inferred read-only `shell` / `gpt_shell` calls remain free to run while update locks are held. With `dir_lock.enable = false`, all shell calls are ordinary read-write commands and no access-mode chip is published for the UI. User `!` shell commands are UI commands, not agent tool calls, and are intentionally excluded.

The shell read-write inference and automatic lock acquisition are atomic: if a
manual lock is force-unlocked or otherwise removed before the automatic lock is
granted, the queued shell call falls back to inferred read-only rather than
running as read-write under stale lock coverage.

`dir_lock.enforce_ro_bind` defaults true and is defense-in-depth for inferred
read-only shell calls. When enabled, ext-shell requires a read-only bind mount
for the shell cwd and fails the shell call if native isolation is unsupported or
cannot be installed. If users explicitly disable it, inferred read-only remains
advisory.


## UI behavior

Blocked ext-shell tool calls emit `ToolProgress` with a live `ToolDisplay` update that shows the directory or directories being waited on. `dir_lock` terminal success and failure displays also include the relevant directory when it is known. Those displayed directories are valid inputs to `/shell-dir-force-unlock`; the action releases overlapping manual locks, so either an ancestor or child lock can be cleared from the waiting path. Normal foreground and auto-background behavior still applies because the harness sees the call as running until the extension sends a terminal event.


## Caveats

- Shell locking is advisory. An inferred read-write shell command can mutate paths outside its `cwd` using absolute paths or command-specific flags. Inferred read-only is not a hard sandbox unless native read-only bind isolation is active and effective.
- Filesystem-backend locks are released when the owning ext-shell process exits.
  If a spawned shell descendant deliberately detaches and keeps mutating files
  after ext-shell exits, another Tau instance may proceed while that detached
  process is still running.
- `edit` creates to missing parents are safe but less precise because the exact final parent does not exist yet.
- Same-owner reentry can keep other agents waiting for as long as the owner keeps a manual lock. That is intentional manual-lock behavior, not starvation inside the FIFO queue.
- Out-of-tree non-shell tools no longer get harness update/exclusive serialization. They need their own coordination if they mutate shared state.
