---
name: tau-tool-verification-directory-locks
description: >
  Use this skill when verifying Tau dir_lock behavior, including manual and automatic lock scopes, conflict matrices, lock wait metadata, cancellation, force unlock, and lifecycle cleanup.
advertise: false
---

# Tau Tool Verification Directory Locks

Load `tau-tool-verification` first for the shared output structure, escaping,
line handling, tool-description, availability, and reporting guidelines.
This skill supplies the focused verification guidance for this tool group.

### Directory locking verification plan

Generic `shell.cwd` and ChatGPT-facing `shell_command.workdir` arguments are
invocation-local. They may select lock inference and execution scope for that
call but must never update remembered workdir metadata.

Use this plan when asked to verify ext-shell directory locking, `dir_lock`, or the interaction between locking, filesystem mutation tools, same-owner shell coverage, backgrounding, `cancel`, and `wait`. Directory locking is optional and advisory. It is owned by `tau-ext-shell`, not the harness or `agent_start`.

Create a fresh scratch tree in `/tmp`, such as `/tmp/tau-dir-lock-verification.*`, with at least these directories: `root/a`, `root/a/child`, `root/b`, and `other`. Put small files in `root/a/file.txt` and `root/b/file.txt`. Use unique nonces in file contents and messages. Never run destructive shell commands outside the scratch tree.

Run the first check with default ext-shell config and confirm `dir_lock` is
disabled by default unless configuration opted in. Also start a fresh Tau session
or configuration with `dir_lock: { enable: true }` before running the locking
behavior checks below. When `dir_lock.enable` is false, confirm the tool is
disabled or unavailable and mutation tools do not wait on directory locks.

When locking is enabled, verify all of these behaviors:

* `dir_lock` accepts only `command: update` and `command: unlock` with an existing directory.
* Directories are canonicalized before locking. Relative paths, `.` components,
  and symlinked directories behave as the canonical absolute directory.
  Provider output includes `canonical_directory` only when canonicalization
  changes the submitted spelling; an already-canonical invocation returns only
  `locked`, while UI display still names the canonical path.
* Missing directories and regular files are rejected before any lock is acquired.
* Manual locks are owner-scoped by `agent_id`; a different agent cannot unlock them unless it passes `owner_agent_id` for an explicit force-unlock.
* Repeated `update` by the same agent on the same canonical directory, an ancestor, or a child is an error. It should return `error: dir_lock_duplicate` with details headers including `blocking_directory`, `requested_directory`, and `lock_owner_id`, plus a short text payload in `output`. Same-agent automatic writer reentry under a manual lock should still complete, including while another same-agent mutating tool under that lock is still running.
* Ancestor and child directories conflict both ways. Sibling directories do not conflict, even when a blocked waiter for another subtree is already queued.
* Reads stay free: `read`, `grep`, `find`, and `ls` complete while an update lock is held.
* Mutating filesystem tools participate when enabled: `edit` and `apply_patch` wait on conflicting locks.
* `shell`/`gpt_shell` do not infer read/write mode from the command text. Without same-owner manual-lock coverage they are inferred read-only and bypass conflicting update locks; with matching same-owner manual `dir_lock` coverage they run as covered read/write shell commands and keep that owner's lock active.
* Lock waiters do not consume the ext-shell worker semaphore before their lock is available. A large number of blocked lock waiters should not prevent unrelated reads from running.
* A mutating tool that waits more than 5s and then acquires its automatic lock reports `lock_wait_duration_seconds` in its final result or error details. Fast, unblocked, canceled, and abandoned lock paths omit it.
* Waiting on an idle manual lock eventually returns an abandoned-lock error. It should return `error: dir_lock_abandoned` with details headers including `blocking_directory`, `lock_owner_id`, `idle_seconds`, and `held_seconds`, plus a clear text payload in `output`. Active same-owner mutating tools under the lock should prevent this abandoned-lock error.
* Waiting tool UI/status includes the directory or directories being waited on. `dir_lock` success and failure UI/status should also include the relevant directory when known, and successful lock/unlock status should use the normal `ok` chip.
* The `:shell-dir-force-unlock DIRECTORY` UI action is published by ext-shell and force-releases manual locks overlapping that canonical directory, regardless of owner.
* `agent_start` agents are independent owners. A parent lock does not automatically cover a delegate, and a delegate lock does not belong to the parent.
* User `!` shell commands are excluded from this lock path.

#### Phase 1: basic manual lock behavior

With `dir_lock.enable` true, call `dir_lock update` on a relative path like
`root/a/../a`. Expect success with `canonical_directory` in the provider result
and the canonical path in display. Unlock it, then reacquire with that exact
canonical path and expect only `locked` in the provider result while display
still names the path. Unlock again. Do not issue the canonical update while the
noncanonical spelling remains held: that is correctly a same-owner duplicate.

Call `dir_lock update` on a missing directory and on `root/a/file.txt`. Expect tool errors. Then call `dir_lock update` twice on `root/a` from the same agent. The second update should error and mention the already-held lock. Also call `dir_lock update root/a/child` and `dir_lock update root` from that same agent while `root/a` is held; both should error. Start a delegate that tries to create `root/a/child/blocked.txt` with `edit` and reports to `user` after it succeeds. The delegate should wait. Call `dir_lock unlock` once from the original agent; the delegate should complete. A second `unlock` should error. Also verify that a different agent cannot unlock Agent A's lock without `owner_agent_id`, but can force-unlock it when `owner_agent_id` is Agent A.

Also verify same-owner reentry: while the original agent holds `root/a`, run a same-agent `edit` inside `root/a`. It should complete instead of deadlocking on its own manual lock. Then start a same-agent `shell` in `root/a` that sleeps briefly before exiting; while that shell is still running, run another same-agent `edit` inside `root/a`. The edit should complete before the shell exits and should not emit directory-lock waiting progress.

#### Phase 2: reads remain unblocked

Hold a manual lock on `root/a`. While it is held, run `read root/a/file.txt`,
`grep` against `root/a`, `find` under `root/a`, and `ls root/a`. These should
complete promptly and should not wait for unlock. From a different agent, also
run a `shell` command with `cwd: root/a` and a `gpt_shell` command with
`workdir: root/a` that would write a sentinel
file. Current shell semantics infer this as read-only because the caller does
not hold a matching manual lock, so it should not wait on Agent A's lock. If
`enforce_ro_bind` is enabled and native read-only isolation is available, the
write should fail as a normal shell result and the sentinel should remain
absent. If `enforce_ro_bind` is enabled but native isolation is unsupported or
cannot be installed, the shell call should fail or start-error rather than
silently degrading to read/write. Only when `enforce_ro_bind` is disabled may
the command write despite the advisory lock. Report that no-coverage behavior as
the expected shell bypass caveat, not as an update-lock wait failure.

#### Phase 3: automatic lock scopes

For each automatically locked filesystem mutation tool, hold the relevant manual lock from one agent and run the tool from a different delegate. Confirm it waits until the lock is released. Verify shell separately because it uses same-owner manual-lock coverage rather than command-text mutation inference:

* `edit`: lock the target file parent. Existing final symlinks should be followed to the real edited file. Missing-parent creates like `root/a/new/dir/file.txt` should wait on the deepest existing ancestor and then create parents after unlock.
* `apply_patch`: use a patch that touches one file under `root/a` and one under `root/b`. If `root/a` is locked, neither change should be applied before the lock is granted. After unlock, both changes should appear together from the patch invocation. Separately, verify the patch safety cases from `tau-tool-verification-file-shell`: existing-file `Add File` and move-to-existing-destination failures preserve all affected files, while successful multi-file patches produce structured UI diffs for every changed UTF-8 file.
* `shell` and `gpt_shell`: verify the current manual-lock coverage rule. Without a matching same-owner manual lock, commands are inferred read-only and should bypass another agent's update lock. With a matching same-owner manual lock on the canonical call-local `cwd` (`shell`) or `workdir` (`gpt_shell`), or an ancestor, shell commands are covered by that owner's lock and should keep the lock active for abandonment/liveness purposes.

For `shell`, also verify the advisory limitation: a command with `cwd: other` that writes to an absolute path under `root/a` is not protected by a `root/a` manual lock unless the caller holds matching manual-lock coverage for the relevant command scope. Report this as expected advisory behavior, not a lock failure.

#### Phase 4: ancestor, child, and sibling conflict matrix

Use separate agents so owner reentry does not hide conflicts. Verify these cases:

* Agent A holds `root/a`; Agent B tries `dir_lock update root/a/child`. B waits until A unlocks.
* Agent A holds `root/a/child`; Agent B tries `dir_lock update root/a`. B waits until A unlocks.
* Agent A holds `root/a`; Agent B mutates `root/b`. B should not wait.
* Agent A holds `root/a`; Agent B tries `dir_lock update root/a/child`; Agent C then tries `dir_lock update other`. C should not wait behind B because the requested paths do not overlap. B should remain queued until A unlocks.
* Agent A holds `root/a`; Agent B tries `dir_lock update root`; Agent C then tries `dir_lock update root/b`. C should not acquire before B because C's requested path overlaps B's earlier queued request. After A unlocks, B should acquire first; C should remain blocked until B unlocks.

The FIFO check is the starvation guard only among overlapping path requests. If an unrelated C waits behind B, record it as head-of-line blocking. If an overlapping C completes before B while B is already queued earlier, record it as a fairness bug.

#### Phase 5: user force-unlock action

Hold `root/a` from Agent A. Start Agent B mutating `root/a/child` and wait until the UI shows it is waiting on `root/a/child` or another canonical child directory. Invoke `:shell-dir-force-unlock root/a/child` from the UI. Expected: the action output names the released lock owner, Agent B completes, and a later `dir_lock unlock root/a` from Agent A errors because the manual lock was already force-released.

Also test the reverse overlap: Agent A holds `root/a/child`, Agent B waits on `root/a`, and `:shell-dir-force-unlock root/a` releases the child lock. Calling the action for a directory with no overlapping manual locks should return a clear action error. Running automatic locks should not be force-released; wait for those tools or cancel them normally.

#### Phase 6: cancellation and background behavior

Hold `root/a` from one agent. Start a delegate or tool call using `edit` or
`apply_patch` that would create a sentinel file under `root/a`. Let it wait long
enough to show the waiting directory in the UI; if it backgrounds, record the
placeholder ID. Call `cancel` on the waiting tool call ID when the harness
exposes it as cancellable. Expected: cancel is accepted, the waiting lock request
is removed, `wait` returns a canceled result if the call backgrounded, and the
sentinel file is still absent after the lock is later released.

Do not count cancellation of `edit` as required unless the harness exposes those call IDs as cancellable in that run. The important lock-specific behavior is that a waiting lock request can be canceled and does not run later after unlock.
Cancellation remains authoritative during the handoff from a removed waiter to
an acquired automatic guard. If cancellation is processed before effect start,
the mutation must remain absent and exactly one cancelled terminal should
complete the call; cancellation after effect start does not roll back changes.

#### Phase 7: agent lifecycle cleanup

Start a delegate that calls `dir_lock update root/a`, reports that it acquired the lock, and then exits without unlocking. After the delegate returns its final answer, a different agent should be able to lock or mutate `root/a` without waiting forever, even if Tau keeps the delegate's session agent loaded for history. If the lock remains stuck after the delegate start result, record it as a lifecycle cleanup bug. If a later `SessionAgentUnloaded` event is visible, it should also release any remaining manual locks for that agent.

Also test session shutdown if practical: locks from the old session must not affect a fresh session.

#### Phase 8: abandoned-lock liveness

Run this phase only when specifically testing stale-lock behavior; it intentionally waits for the liveness timer. Hold `root/a` from Agent A, do not use it, and start Agent B mutating `root/a/child`. After the liveness interval and stale threshold, Agent B should get a tool error instead of waiting forever. It must use `error: dir_lock_abandoned`; details headers must include the blocking canonical directory, Agent A's id as `lock_owner_id`, `idle_seconds`, and `held_seconds`; the `output` payload should explain that the lock may be abandoned and can be resolved by messaging the owner or force-unlocking. Repeat with Agent A running a long same-agent `shell` under `root/a`; the abandoned-lock error should not fire while that shell is active.

#### Reporting format for directory locking verification

Report concise but complete findings:

* Whether `dir_lock` was disabled by default unless opted in, and whether enabling/disabling it by config behaved as expected.
* Exact outputs or errors for canonicalization, missing directory, non-directory, same-agent double update, double unlock, wrong-owner unlock, and `owner_agent_id` force-unlock.
* Whether same-agent automatic writer reentry still worked while manual double updates errored, including reentry while a same-agent shell under the manual lock was still running.
* Whether reads stayed unblocked.
* For each automatically locked filesystem mutation tool, whether it waited on the expected directory and completed only after unlock; for `shell`/`gpt_shell`, whether no-coverage commands bypassed update locks and same-owner manual-lock-covered commands kept the lock active.
* Whether waiting UI/status showed the blocked directory, whether `dir_lock` failures showed the target directory, and whether auto-background plus `wait` behaved normally.
* Whether slow acquired lock waits reported `lock_wait_duration_seconds`, and whether quick/no-wait, canceled, and abandoned paths omitted it.
* Whether `:shell-dir-force-unlock DIRECTORY` was available, released overlapping manual locks, reported owner details, and left automatic locks alone.
* Whether unrelated later waiters could proceed despite an earlier blocked waiter, while later overlapping waiters still stayed behind the earlier conflicting waiter.
* Whether cancellation removed a waiting lock request and prevented the delayed mutation.
* Whether abandoned-lock liveness errors used `error: dir_lock_abandoned`, structured details headers for `blocking_directory`, `lock_owner_id`, `idle_seconds`, and `held_seconds`, and an explanatory `output` payload, and whether active same-owner tools suppressed the abandoned-lock error.
* Whether delegate final-answer, agent unload, and session shutdown released manual locks.
* Any advisory-shell caveat observed, especially commands writing outside their locked `cwd` or into a locked directory from another `cwd`.
