---
name: tau-tool-verification
description: >
  Use this skill when asked to verify Tau harness tools or tool output behavior,
  especially read, edit, shell, line-oriented output, truncation, metadata
  headers, UTF-8 handling, diffs, timeouts, or skill/tool conformance.
---

# Tau Tool Verification

Use when asked to verify Tau skills.

If not explicitly stated, assume the user means `read`, `edit` and `shell` tools.

## Goal

Your goal is to verify if basic Tau harness tools still work as expected,
and conform to our standards and guidelines.

## Guidelines

### Tool result output structure
All tools should return a HTTP-protocol-like structure:

```
header-1: value-1
header-2: value-2
...
header-n: value-n

multi-line-payload
```

With a single empty line separating headers from the main payload.

`multi-line-payload` can be arbitrary, but line-oriented output typically uses
`<prefix>(optional-per-line-flags) <line-content>` structure. If that's the case
the tool description should mention it.

Tool outputs with non-trivial fields encoded into line-oriented
payloads should include a `format` header describing field order and names.
For example, an email listing can use:

```text
format: uid date from flags access attachments subject...

6212 2016-04-23T17:32:52Z builds@travis-ci.org seen,redacted preview 0 Hi there, from us
```

The `...` suffic on last field in the format is used to indicate it's a multi-word field that will extend till the end of the line.

Tool implementation must take care ensuring newlines and special characters are stripped from field values, and empty values use some placeholders (e.g. `-`) to avoid breaking the meaning of each line.

Many headers are optional, and skipped for their default most natural values
for token efficiency. Keep tool output compact: include only non-default,
non-redundant values that help the agent decide what to do next. Do not emit
aliases or duplicate fields that carry the same information.

Do not include headers that are straight copies of tool invocation arguments.
The calling agent already knows the arguments it sent, so echoing them wastes
context and makes the meaningful result harder to scan. Only report a requested
path, query, command, or similar argument when the tool has transformed it into
new information, such as a canonicalized path that differs from the input.

### Layered escaping policy

Tools must semantically escape untrusted metadata fields before composing
model-visible text. This includes paths, filenames, identifiers, owner names,
queries, commands when shown as metadata, and any other field whose bytes come
from the workspace, filesystem, user config, or an extension peer. Escaping is
local to the tool because the tool knows which substrings are metadata and which
substrings are user/file payload that should remain literal.

Line-oriented outputs must never let metadata inject extra records, headers, or
status lines. Escape at least `\\`, newlines, carriage returns, tabs, and other
control characters in metadata fields; use explicit flags such as `escaped` or
`invalid-utf8` when that helps the caller understand that displayed text is not
byte-exact. Do not over-escape file contents or command output just because they
are untrusted; those payloads have separate line-prefix, truncation, and UTF-8
handling rules.

Central provider-visible rendering should still apply a last-resort safety
invariant when structured tool responses are rendered into model input, but that
is defense-in-depth. It is not a substitute for tool-local semantic escaping,
because a central renderer cannot reliably know whether an arbitrary string is a
path, a header value, a status label, or content.

Terminal/UI sanitization is a separate layer. UI code must protect terminal
state and layout from control sequences, but UI escaping does not make
provider/model-visible text safe, and provider-visible escaping does not replace
terminal sanitization.

### Common patterns

`read` range operations use inclusive `start_line` and `end_line` fields. `edit` range operations use half-open `start_line` and `end_line_exclusive` fields.
Newlines are assumed to be `\n`, but other styles are supported
and displayed as `crlf` (`\r\n`), `cr` (`\r`) or `no_nl` (missing trailing newline).
This applies to both `read` line-number prefixes and `shell` stdout/stderr prefixes.

Lines containing invalid UTF-8 bytes should show Unicode replacement characters and an `invalid-utf8` flag,
so useful surrounding content remains visible while the agent knows the bytes were not exact.
Lines which are too long show a `truncated` flag and have content skipped.

Total outputs that are too long are truncated; `truncated: true`,
`total_lines: {lines}` and `total_bytes: {bytes}` headers are added.
These total headers are omitted when output is not truncated, except `read` may report `total_lines: 0` and `total_bytes: 0` for an empty file.

When output is truncated due to line number limit, first and last 1000 lines
should be shown with `...` line separating them, instead of usual line prefix.
If a single line would exceed the byte budget (currently 50 KB for
`read`/`shell`), show only the line prefix plus `(truncated)` rather than
partial content.


### Tool descriptions

Tool description should be short but informative. They should mention the line prefix meaning, if used in the tool. They should mention line and byte limits.


### Tool-specific guidelines

The output of `read` and `shell` is intentionally similar, and should support
the same semantics. The meaning of the line prefix is different: line number vs stdout/stderr information

`read` supports either one top-level inclusive `start_line`/`end_line` range or a `ranges` array of up to 100 inclusive `{ start_line, end_line }` objects. Multi-range output uses the same line-number prefixes as normal read output, with exactly one empty line between requested chunks; overlapping ranges are allowed and return redundant chunks. Verify that mixed `ranges` plus top-level range arguments are rejected. `read` renders only actual file lines: `start_line` past the last actual line is an error, except `start_line: 1` is allowed for an empty file and returns empty content with zero totals. An `end_line` past EOF returns the available suffix rather than an error.
`shell` tool will add `duration_seconds: {number}` header for commands that took longer
than 5s to execute. Whole-second precision is acceptable; finer precision is
not needed. Reported durations are approximate, and can include overheads and
latencies of internal components.

Mutating ext-shell tools that wait more than 5s to acquire an automatic directory update lock should add `lock_wait_duration_seconds: {number}` to the final result or error details. Use the same approximate whole-second semantics as `duration_seconds`. Omit this header for waits of 5s or less, for tools that did not wait, and for canceled or abandoned waiters that never acquired the lock.

`shell` tool should return non-zero exits and timeouts as structured command
results with output details, not as tool invocation errors. It should reliably
timeout operations that take longer than timeout argument, but currently 100%
reliable child process termination is not implemented and will require advanced
techniques to implement in the future (e.g. cgroups).

#### Shell read-only mode

When verifying `shell`, also verify `mode: ro`. Use a fresh scratch directory
under `/tmp`; create an input file in it; then run a `shell` command with
`cwd` set to that directory and `mode: ro` that reads the input file and then
tries to create or overwrite another file in the same directory, e.g.
`cat input.txt; touch should-not-exist.txt`.

Expected on Linux platforms that support the native mount namespace setup:
the read succeeds, the write fails with a non-zero shell result, and the target
file is still absent after the command. The failure should be reported as a
normal shell command result, not a tool invocation error. If the platform or
container policy does not support read-only bind mounts, `mode: ro` may degrade
to a normal shell command; report that the ro-bind enforcement was unavailable
instead of treating the shell invocation itself as broken.

`edit` is line-oriented. Each edit entry must include `start_line`, `end_line_exclusive`, `newText`, and `context_line`; it replaces the original half-open line range `start_line..end_line_exclusive` with `newText` as whole replacement lines. Empty insertion ranges use `start_line == end_line_exclusive`; non-empty replacements cover `start_line` through `end_line_exclusive - 1`. To replace read output lines A through B, use `start_line: A` and `end_line_exclusive: B + 1`. All edit ranges use the original file numbering as if applied simultaneously, so the tool must reject overlapping ranges before changing the file. Unlike `read`, `edit` must not clip ranges: both line slots must be at most `total_lines + 1`, and `end_line_exclusive` must be at least `start_line`. When non-empty `newText` lacks a trailing line ending, `edit` normalizes it into a full line using surrounding/replaced content as needed; explicit line endings in `newText` are preserved, so mixed line endings are allowed.
`edit` supports file creation: missing files are treated as empty, and missing parent directories are created only after the request validates. To create a file, insert with `start_line: 1`, `end_line_exclusive: 1`, and an empty `context_line`. The model-visible result should stay minimal: `edits`, `changed`, `new_max_valid_start_line`, and `total_bytes`; `new_max_valid_start_line` is after-edit state and must not be confused with original range validation.

`edit` requires a per-entry `context_line` string matching the original line immediately before `start_line`, excluding any line ending. Use an empty `context_line` when `start_line` is 1. EOF appends to a non-empty file must use the original last line as `context_line`; empty/missing-file creation uses an empty `context_line`. For ease of agent use, trailing literal `\r` and `\n` characters in the supplied context line are accepted and trimmed before matching; embedded `\r` or `\n` characters remain malformed. A malformed context line, missing context line, or context-line mismatch must leave the file unchanged. A mismatch returns read-like `line-numbered content` details around the expected context line plus up to 10 existing lines before and after it, with invalid UTF-8 and truncation handled like `read`; the BOF virtual context line is not rendered as a fake numbered line and is reported as `context_line_number: 0`.
`edit` allows at most 100 edit entries per call. Requests with more entries must error out immediately before reading, writing, or creating parent directories. Invalid ranges, overlapping ranges, missing `newText`, missing or malformed `context_line`, malformed line fields, and context-line mismatches must leave the file unchanged. Error details should not echo raw edit requests; only purpose-built recovery details such as context-line mismatch context should be included.
`apply_patch` should follow safe patch semantics rather than shell `patch` clobber semantics. Verify `Add File` rejects an already-existing destination and preserves its original content. Verify move hunks reject moves whose destination already exists, preserving both the source and the existing destination; a move is distinct from an explicit update/delete and must not silently clobber another file. Context, line, hunk, add, delete, and move validation failures should not silently mutate unrelated content, and all file changes that are applied before a later partial failure must be reported clearly.

`apply_patch` output should stay compact and should not echo the full patch text back to the agent. For UTF-8 files it mutates, the tool should attach structured UI-only diffs; multi-file patches should attach one structured diff per changed file. When a later hunk fails after earlier hunks have already been applied, the agent-visible error must include structured partial-mutation details for the files/paths that changed where applicable, while the UI still receives diffs for those applied UTF-8 changes. Invalid UTF-8 or binary-like files should not produce misleading text diffs; report any missing, duplicated, or agent-visible raw diff payloads as tool-output regressions.

Other commands should adhere to pre-existing conventions and naming used in
standard tools.


### Background tools and `wait`

Some tools can run in the background. The agent first receives a synthetic tool result with `kind: background_placeholder` saying:

```
tau_internal: true

Tool call `<tool_call_id>` is running in the background.
```

When the real tool finishes, Tau queues an internal, UI-hidden prompt for the owning conversation saying:

```
[tau-internal] Tool call `<tool_call_id>` is complete.
```

This prompt is model-visible only if it reaches the agent before the completion is consumed by `wait`. If the agent is already in a model turn, the prompt may sit in the pending prompt queue. A later `wait` can consume the completed result and suppress/remove that queued prompt before the model ever sees it. This is expected and is not a delivery failure.

The agent can call `wait({"tool_call_id": "..."})` to collect that specific real result, or `wait({})` to wait for the first background completion in the current conversation. The no-arg form is conversation-scoped: it must not consume completions from parent, child, or sibling conversations. The tool description shown to agents often says not to call `wait` until they know the tool call has completed. This is an optimization to avoid wasting tokens: for foreground calls, the normal tool call result will arrive without an extra `wait`, and for background calls Tau will wake the agent when the completion prompt is delivered. It is not a technical requirement. The `wait` tool must work well when called for tool calls that are still running, and it must have reasonable semantics in all cases. If `wait` is used for a backgrounded call before completion, Tau suppresses that internal completion prompt while still emitting the real background result/error event. If `wait` consumes an already-completed result before its queued completion prompt is delivered to the model, Tau also suppresses/removes that prompt. If `wait({})` consumes a completion, it suppresses the normal `[tau-internal] Tool call ... is complete.` prompt for that completion and returns an `original_tool_call_id: <tool_call_id>` provider-visible header so the agent knows which background call was collected.

Current background timing: most tools background after about 5 seconds, `agent_start` backgrounds instantly, and `wait` itself never backgrounds. This may change; when verifying, report if observed behavior differs.

Slow `agent_start` calls should include the same `duration_seconds` header semantics as `shell`: omit fast calls, include approximate whole seconds for calls that took longer than about 5 seconds, and allow internal overheads and jitter. Delegate duration measures parent-observed delegate wall-clock time, not only inner tool runtime. It includes sub-agent model latency, tool scheduling, inner background/wait turns, final response latency, and is rounded up to whole seconds. Verify this both for direct background agent_start results and for agent_start results collected through `wait`.

When asked to verify the `agent_start` tool, also verify delayed `message` delivery to a live delegated sub-agent whose own tool turn is parked behind a backgrounded tool. This is a delegate-specific regression path, not only a `message` tool test. Use a delegate prompt that first runs `sleep 30`, then after the background placeholder requests a second shell command `sleep 5`, and asks it to report to `user` if it receives a parent message. After the first shell backgrounds and the second shell request is queued, send `message` to the delegate `sub_agent_id` with a nonce. Expected: `Message sent`, the queued `sleep 5` is terminalized internally, and the delegate promptly reports receiving the nonce instead of staying stuck until `sleep 30` finishes. If event logs are available, confirm `AgentMessage`, `ToolCancelled` for the not-yet-started queued call, and a new `AgentPromptCreated` for the delegate message prompt. Treat omission of this scenario as incomplete `agent_start` verification.

Also verify the active-`wait` variant of the same scenario. Use a delegate prompt that starts a long backgroundable tool, then calls `wait` on that tool call ID before it completes. While the delegate is blocked in `wait`, send `message` to the delegate `sub_agent_id` with a nonce. Expected: `Message sent`, the delegate's `wait` returns promptly with a `tau_internal: true` interruption result saying new input is queued, and the delegate receives the hidden message prompt without waiting for the original background tool to complete. If event logs are available, confirm the wait `ToolResult` appears before the message-driven follow-up `AgentPromptCreated`.

Because `agent_start` enables a persistent watch, these message-delivery probes can produce later watch notifications if the child leaves an earlier background tool running. For example, after an active-`wait` interruption, the original sleep may complete later, queue a normal background-completion prompt in the child, and the child may answer something like `Received.`. That is not a duplicate watch notification by itself; it is a later child turn caused by the delayed inner completion. If the verifier no longer wants notifications from that child after the success nonce, explicitly call `agent_watch({"agent_id":"<sub_agent_id>","enable":false})`.

A completed background result is consumed by the first successful `wait`. Later waits for the same id should fail with an already-consumed error. Parallel duplicate waits on the same id race; at most one should receive the result, and the rest should fail. Parallel duplicate no-arg waits in the same conversation should also fail clearly because only one waiter can consume the next completion. The exact error depends on timing: an in-progress duplicate-wait error, an already-consumed error, or another clear race-related error can be acceptable if only one wait receives the result.
A completed background result is consumed by the first successful `wait`. Later waits for the same id should fail with an already-consumed error. Parallel duplicate waits on the same id race; at most one should receive the result, and the rest should fail. Parallel duplicate no-arg waits in the same conversation should also fail clearly because only one waiter can consume the next completion. The exact error depends on timing: an in-progress duplicate-wait error, an already-consumed error, or another clear race-related error can be acceptable if only one wait receives the result.


### Directory locking verification plan

Use this plan when asked to verify ext-shell directory locking, `dir_lock`, or the interaction between locking, mutating shell tools, backgrounding, `cancel`, and `wait`. Directory locking is optional and advisory. It is owned by `tau-ext-shell`, not the harness or `agent_start`.

Create a fresh scratch tree in `/tmp`, such as `/tmp/tau-dir-lock-verification.*`, with at least these directories: `root/a`, `root/a/child`, `root/b`, and `other`. Put small files in `root/a/file.txt` and `root/b/file.txt`. Use unique nonces in file contents and messages. Never run destructive shell commands outside the scratch tree.

Run the first check with default ext-shell config and confirm `dir_lock` is enabled by default. Also start a fresh Tau session with ext-shell config `dir_lock: { enable: false }` and confirm the tool is disabled and mutating tools no longer wait on directory locks. Use explicit `dir_lock: { enable: true }` only when you need to override a previously disabled test config.

When locking is enabled, verify all of these behaviors:

* `dir_lock` accepts only `command: update` and `command: unlock` with an existing directory.
* Directories are canonicalized before locking. Relative paths, `.` components, and symlinked directories should report or behave as the canonical absolute directory.
* Missing directories and regular files are rejected before any lock is acquired.
* Manual locks are owner-scoped by `agent_id`; a different agent cannot unlock them unless it passes `owner_agent_id` for an explicit force-unlock.
* Repeated `update` by the same agent on the same canonical directory, an ancestor, or a child is an error. It should return `error: dir_lock_duplicate` with details headers including `blocking_directory`, `requested_directory`, and `lock_owner_id`, plus a short text payload in `output`. Same-agent automatic writer reentry under a manual lock should still complete, including while another same-agent mutating tool under that lock is still running.
* Ancestor and child directories conflict both ways. Sibling directories do not conflict, even when a blocked waiter for another subtree is already queued.
* Reads stay free: `read`, `grep`, `find`, and `ls` complete while an update lock is held.
* Mutating tools participate when enabled: `edit`, `apply_patch`, `shell`, and `gpt_shell` wait on conflicting locks.
* Lock waiters do not consume the ext-shell worker semaphore before their lock is available. A large number of blocked lock waiters should not prevent unrelated reads from running.
* A mutating tool that waits more than 5s and then acquires its automatic lock reports `lock_wait_duration_seconds` in its final result or error details. Fast, unblocked, canceled, and abandoned lock paths omit it.
* Waiting on an idle manual lock eventually returns an abandoned-lock error. It should return `error: dir_lock_abandoned` with details headers including `blocking_directory`, `lock_owner_id`, `idle_seconds`, and `held_seconds`, plus a clear text payload in `output`. Active same-owner mutating tools under the lock should prevent this abandoned-lock error.
* Waiting tool UI/status includes the directory or directories being waited on. `dir_lock` success and failure UI/status should also include the relevant directory when known, and successful lock/unlock status should use the normal `ok` chip.
* The `/shell-dir-force-unlock DIRECTORY` UI action is published by ext-shell and force-releases manual locks overlapping that canonical directory, regardless of owner.
* `agent_start` agents are independent owners. A parent lock does not automatically cover a delegate, and a delegate lock does not belong to the parent.
* User `!` shell commands are excluded from this lock path.

#### Phase 1: basic manual lock behavior

With default config or `dir_lock.enable` true, call `dir_lock update` on a relative path like `root/a/../a`. Expect success and a canonical absolute directory in the result/display. Call `dir_lock unlock` for the same path. Expect success.

Call `dir_lock update` on a missing directory and on `root/a/file.txt`. Expect tool errors. Then call `dir_lock update` twice on `root/a` from the same agent. The second update should error and mention the already-held lock. Also call `dir_lock update root/a/child` and `dir_lock update root` from that same agent while `root/a` is held; both should error. Start a delegate that tries to create `root/a/child/blocked.txt` with `edit` and reports to `user` after it succeeds. The delegate should wait. Call `dir_lock unlock` once from the original agent; the delegate should complete. A second `unlock` should error. Also verify that a different agent cannot unlock Agent A's lock without `owner_agent_id`, but can force-unlock it when `owner_agent_id` is Agent A.

Also verify same-owner reentry: while the original agent holds `root/a`, run a same-agent `edit` inside `root/a`. It should complete instead of deadlocking on its own manual lock. Then start a same-agent `shell` in `root/a` that sleeps briefly before exiting; while that shell is still running, run another same-agent `edit` inside `root/a`. The edit should complete before the shell exits and should not emit directory-lock waiting progress.

#### Phase 2: reads remain unblocked

Hold a manual lock on `root/a`. While it is held, run `read root/a/file.txt`, `grep` against `root/a`, `find` under `root/a`, and `ls root/a`. These should complete promptly and should not wait for unlock. Then start a conflicting `shell` with `cwd: root/a` and command `python3 -c 'open("shell-waited.txt", "w").write("locked")'`. It should wait. Leave it blocked for more than 5s, then unlock `root/a`; the shell should run, create the file, and include `lock_wait_duration_seconds` in the final result.

If the `shell` waits long enough to background, call `wait` after unlocking and confirm the real shell result is returned normally with the same `lock_wait_duration_seconds` header.

#### Phase 3: automatic lock scopes

For each mutating tool, hold the relevant manual lock from one agent and run the tool from a different delegate. Confirm it waits until the lock is released:

* `edit`: lock the target file parent. Existing final symlinks should be followed to the real edited file. Missing-parent creates like `root/a/new/dir/file.txt` should wait on the deepest existing ancestor and then create parents after unlock.
* `apply_patch`: use a patch that touches one file under `root/a` and one under `root/b`. If `root/a` is locked, neither change should be applied before the lock is granted. After unlock, both changes should appear together from the patch invocation. Separately, verify the patch safety cases from the tool-specific section: existing-file `Add File` and move-to-existing-destination failures preserve all affected files, while successful multi-file patches produce structured UI diffs for every changed UTF-8 file.
* `shell` and `gpt_shell`: lock the canonical `cwd`. A command with `cwd: root/a` should wait on a `root/a` lock.

For `shell`, also verify the advisory limitation: a command with `cwd: other` that writes to an absolute path under `root/a` is not expected to wait on the `root/a` lock. Report this as expected advisory behavior, not a lock failure.

#### Phase 4: ancestor, child, and sibling conflict matrix

Use separate agents so owner reentry does not hide conflicts. Verify these cases:

* Agent A holds `root/a`; Agent B tries `dir_lock update root/a/child`. B waits until A unlocks.
* Agent A holds `root/a/child`; Agent B tries `dir_lock update root/a`. B waits until A unlocks.
* Agent A holds `root/a`; Agent B mutates `root/b`. B should not wait.
* Agent A holds `root/a`; Agent B tries `dir_lock update root/a/child`; Agent C then tries `dir_lock update other`. C should not wait behind B because the requested paths do not overlap. B should remain queued until A unlocks.
* Agent A holds `root/a`; Agent B tries `dir_lock update root`; Agent C then tries `dir_lock update root/b`. C should not acquire before B because C's requested path overlaps B's earlier queued request. After A unlocks, B should acquire first; C should remain blocked until B unlocks.

The FIFO check is the starvation guard only among overlapping path requests. If an unrelated C waits behind B, record it as head-of-line blocking. If an overlapping C completes before B while B is already queued earlier, record it as a fairness bug.

#### Phase 5: user force-unlock action

Hold `root/a` from Agent A. Start Agent B mutating `root/a/child` and wait until the UI shows it is waiting on `root/a/child` or another canonical child directory. Invoke `/shell-dir-force-unlock root/a/child` from the UI. Expected: the action output names the released lock owner, Agent B completes, and a later `dir_lock unlock root/a` from Agent A errors because the manual lock was already force-released.

Also test the reverse overlap: Agent A holds `root/a/child`, Agent B waits on `root/a`, and `/shell-dir-force-unlock root/a` releases the child lock. Calling the action for a directory with no overlapping manual locks should return a clear action error. Running automatic locks should not be force-released; wait for those tools or cancel them normally.

#### Phase 6: cancellation and background behavior

Hold `root/a` from one agent. Start a delegate or shell call whose mutating `shell` invocation uses `cwd: root/a` and would create a sentinel file. Let it wait long enough to show the waiting directory in the UI; if it backgrounds, record the placeholder ID. Call `cancel` on the waiting shell tool call ID. Expected: cancel is accepted, the waiting lock request is removed, `wait` returns a canceled result if the call backgrounded, and the sentinel file is still absent after the lock is later released.

Do not count cancellation of `edit` as required unless the harness exposes those call IDs as cancellable in that run. The important lock-specific behavior is that a waiting lock request can be canceled and does not run later after unlock.

#### Phase 7: agent lifecycle cleanup

Start a delegate that calls `dir_lock update root/a`, reports that it acquired the lock, and then exits without unlocking. After the delegate returns its final answer, a different agent should be able to lock or mutate `root/a` without waiting forever, even if Tau keeps the delegate's session agent loaded for history. If the lock remains stuck after the delegate start result, record it as a lifecycle cleanup bug. If a later `SessionAgentUnloaded` event is visible, it should also release any remaining manual locks for that agent.

Also test session shutdown if practical: locks from the old session must not affect a fresh session.

#### Phase 8: abandoned-lock liveness

Run this phase only when specifically testing stale-lock behavior; it intentionally waits for the liveness timer. Hold `root/a` from Agent A, do not use it, and start Agent B mutating `root/a/child`. After the liveness interval and stale threshold, Agent B should get a tool error instead of waiting forever. It must use `error: dir_lock_abandoned`; details headers must include the blocking canonical directory, Agent A's id as `lock_owner_id`, `idle_seconds`, and `held_seconds`; the `output` payload should explain that the lock may be abandoned and can be resolved by messaging the owner or force-unlocking. Repeat with Agent A running a long same-agent `shell` under `root/a`; the abandoned-lock error should not fire while that shell is active.

#### Reporting format for directory locking verification

Report concise but complete findings:

* Whether `dir_lock` was enabled by default and could be disabled by config.
* Exact outputs or errors for canonicalization, missing directory, non-directory, same-agent double update, double unlock, wrong-owner unlock, and `owner_agent_id` force-unlock.
* Whether same-agent automatic writer reentry still worked while manual double updates errored, including reentry while a same-agent shell under the manual lock was still running.
* Whether reads stayed unblocked.
* For each mutating tool, whether it waited on the expected directory and completed only after unlock.
* Whether waiting UI/status showed the blocked directory, whether `dir_lock` failures showed the target directory, and whether auto-background plus `wait` behaved normally.
* Whether slow acquired lock waits reported `lock_wait_duration_seconds`, and whether quick/no-wait, canceled, and abandoned paths omitted it.
* Whether `/shell-dir-force-unlock DIRECTORY` was available, released overlapping manual locks, reported owner details, and left automatic locks alone.
* Whether unrelated later waiters could proceed despite an earlier blocked waiter, while later overlapping waiters still stayed behind the earlier conflicting waiter.
* Whether cancellation removed a waiting lock request and prevented the delayed mutation.
* Whether abandoned-lock liveness errors used `error: dir_lock_abandoned`, structured details headers for `blocking_directory`, `lock_owner_id`, `idle_seconds`, and `held_seconds`, and an explanatory `output` payload, and whether active same-owner tools suppressed the abandoned-lock error.
* Whether delegate final-answer, agent unload, and session shutdown released manual locks.
* Any advisory-shell caveat observed, especially commands writing outside their locked `cwd` or into a locked directory from another `cwd`.


### Background tool `cancel`

`cancel` requires `tool_call_id` and never backgrounds. It supports running `agent_start` calls and should support running `shell` calls. A successful cancel request returns `Tool cancellation requested`, emits a harness notice event containing `tool call cancellation request`, and targets only the requested tool call. Cancellation is async and best effort: the success result only means Tau accepted the request, not that the child process or agent has already stopped. A canceled delegate should complete as a background error so `wait` can observe the cancellation instead of hanging. A canceled shell call should also complete through `wait`, include timing headers if it ran longer than about 5 seconds, and must not keep running to normal `status: 0` completion.

Calling `cancel` for an unknown, completed, or unsupported tool call should return a tool error. Unknown ids should be distinguished from already-completed ids. Calling it twice for the same target should return a tool error like `Tool call already canceled`.

When verifying this behavior, check that the synthetic foreground result is visible to the model, the completion notification is delivered to the model when no wait consumes the completion first, and `wait` returns a completed result once and only once. Completion prompt suppression is expected when a matching `wait` is already active before the background call finishes, and also when a completion prompt has been queued but not yet delivered to the model before `wait` consumes the result. If the tool finishes first and Tau already showed `[tau-internal] Tool call ... is complete.` to the model, a later `wait` can still consume the result and that earlier prompt is not a bug.


### Cancel tool verification plan

Use this plan when asked to verify the `cancel` tool, especially around background `agent_start` calls, `wait`, duplicate requests, and leaked work from a canceled sub-agent. The goal is to prove that cancel targets exactly one running agent_start call, reports success or errors clearly, and leaves no orphaned tool completions behind.

Do not rely on memory. Give every sub-agent a self-contained prompt. A delegated agent starts with a clean context and does not know this skill, the parent conversation, or the IDs of other agents unless you include them in its prompt or later messages.

Create a scratch directory in `/tmp`, such as `/tmp/tau-cancel-verification.*`, before running shell probes. Keep all sleeps short except where a background transition or leak check requires a longer wait.

#### What to verify

Record all of these observations:

* The agent_start placeholder includes `tau_internal: true`, `self_agent_id`, `sub_agent_id`, and the background agent_start tool call ID.
* `cancel` must be called with the agent_start `tool_call_id`, not the `sub_agent_id`.
* A successful cancel returns exactly `Tool cancellation requested` and does not background.
* The harness emits a `harness.notice` event containing `tool call cancellation request` if event logs are available.
* The canceled delegate produces a background error that `wait` can collect.
* `wait({"tool_call_id": id})` returns the canceled result once and only once.
* `wait({})` can collect a canceled completion and includes `original_tool_call_id`.
* Waiting before the delegate has completed suppresses the later model-visible completion prompt. Waiting after a completion prompt was already delivered is still valid, but does not retroactively suppress that prompt.
* Duplicate cancel requests race cleanly: one succeeds, later or parallel ones fail with `Tool call already canceled` or another clear duplicate error.
* Canceling an unknown id, completed agent_start id, unsupported running tool id, empty id, or `sub_agent_id` returns a tool error.
* Canceling one delegate does not cancel a sibling delegate.
* Canceling a long-running shell call works and does not let the command complete normally.
* Slow canceled delegates and shell calls include `duration_seconds` after about 5 seconds. A few seconds of timing overhead is normal and not worth reporting by itself.
* A canceled delegate does not leak completions from its own in-flight or backgrounded inner tool calls into the parent conversation.
* The user-visible UI does not show hidden internal completion prompts unless the current UI settings intentionally expose them.

#### Phase 1: running delegate happy path

Start a shared sub-agent with `agent_start` with this prompt:

```text
You are a Tau cancel-tool verification sub-agent. Goal: stay alive until the parent cancels this agent_start call.

Procedure:
1. Immediately send a message to `user` exactly: `READY cancel-ready-probe: entering long sleep`.
2. Run `sleep 60` using the shell tool.
3. If you are not canceled, final answer exactly: `UNEXPECTED cancel-ready-probe completed without cancellation`.

Do not do anything else.
```

After the placeholder result returns, record `self_agent_id`, `sub_agent_id`, and the agent_start tool call ID. Call `cancel` with that agent_start tool call ID. Expect the foreground result to be exactly:

```text
Tool cancellation requested
```

Then wait for the same tool call ID. Expect a background tool error like:

```text
error: Tool call canceled
self_agent_id: ...
sub_agent_id: ...
```

Call `wait` for the same ID again. Expect an already-consumed error. Call `cancel` for the same ID again. Expect `Tool call already canceled`.

#### Phase 2: no-arg wait and wait suppression

Start another long-sleeping sub-agent with `agent_start`. Cancel it, then call `wait({})`. Expect the canceled error and an `original_tool_call_id` header matching the agent_start call ID.

Start a third long-sleeping delegate. Call `cancel` and `wait({"tool_call_id": id})` in parallel or as close together as possible. Expect `wait` to return the canceled result. The later `[tau-internal] Tool call ... is complete.` prompt for that same call should be suppressed. If the prompt still appears after `wait` was already active for that call, record it as a discrepancy. If the completion prompt appears before the wait call is active, do not count it as a suppression failure.

#### Phase 3: invalid targets and duplicate requests

Verify each error case independently:

* `cancel({"tool_call_id": ""})` returns `` `tool_call_id` must not be empty ``.
* A clearly unknown call ID returns `Unknown tool call id` and echoes `tool_call_id`.
* A completed agent_start ID returns `Tool call is already done`.
* A `sub_agent_id` returns `Unknown tool call id`; this proves the tool wants the agent_start call ID.
* Two parallel `cancel` calls for the same live delegate produce one success and one duplicate-cancel error.

For the completed-agent_start case, spawn a sub-agent with `agent_start` that immediately returns:

```text
You are a Tau cancel-tool verification sub-agent. Return immediately with exactly: `FINAL cancel-completed-probe normal completion`.
```

Wait until the completion prompt arrives, then try to cancel it. After that, call `wait` and verify the normal final answer is still available once.

#### Phase 4: running shell cancellation

Start a shell command long enough to background, such as `sleep 20`. When the shell placeholder gives a tool call ID, call `cancel` for that ID. Expect the foreground result to be exactly `Tool cancellation requested`.

Then call `wait` for the shell call. Expect a canceled or terminated result, not a normal `status: 0` completion. If the command ran longer than about 5 seconds, verify the result includes a `duration_seconds` header. If `cancel` rejects the shell call as not cancellable, or if `wait` later returns normal `status: 0`, record this as a discrepancy because shell cancellation is expected to work.

#### Phase 5: target isolation

Start two sub-agents with `agent_start` in parallel. The target should sleep for a long time. The survivor should sleep briefly and return `FINAL cancel-survivor unaffected`.

Cancel only the target delegate. Then wait for both IDs. Expect:

* Target: `error: Tool call canceled`.
* Survivor: normal final answer.

Any sibling cancellation, missing survivor result, or cross-talk between IDs is a bug.

#### Phase 6: slow cancellation and duration

Start a long-sleeping sub-agent with `agent_start`. Let it run long enough to cross the delegate duration threshold, usually about 6 seconds. Cancel it and wait for the result. Expect the canceled agent_start result to include `duration_seconds` with an approximate whole-second value.

Do not require an exact duration. Internal overhead and scheduling can add a few seconds of jitter; do not report small delays by themselves.

#### Phase 7: nested and inner-tool leak check

This phase is important. A canceled delegate can have its own foreground or background tool call in flight. Canceling the delegate must not leave an orphaned inner tool completion that later wakes the parent conversation.

Start a shared sub-agent with `agent_start` with this prompt:

```text
You are a Tau cancel-tool verification sub-agent for inner-tool leak testing. Goal: start an inner tool call, then be canceled by the parent.

Procedure:
1. Run `sleep 12` using the shell tool.
2. If you are not canceled, final answer exactly: `UNEXPECTED cancel-inner-tool-leak completed without cancellation`.

Do not send messages. Do not do anything else.
```

Let the delegate run long enough for the inner shell call to background, usually about 6 seconds. Then cancel the delegate and wait for the agent_start result. Expect `error: Tool call canceled`.

After the delegate cancel result is consumed, watch for stray completion prompts for any other tool call ID, especially the inner shell call. If a stray `[tau-internal] Tool call ... is complete.` prompt appears, call `wait` for that ID and record the full result. Treat this as a leak unless there is a clear documented reason it belongs to the parent conversation.

If no stray completion appears before the inner `sleep 12` would have finished, record that no leak was observed. This check caught a prior manual discrepancy where a canceled delegate's inner `sleep` later produced a parent-visible completion.

#### Optional event-log checks

If you have direct access to harness event logs, verify:

* Successful cancel emitted `harness.notice` with `tool call cancellation request`.
* The canceled delegate emitted `ToolBackgroundError` with `Tool call canceled`.
* No `AgentPromptSteered` or queued pending prompt remains for canceled nested delegate completions.
* Completed results are consumed once, and the consumed result is not available to later `wait` calls.

#### Reporting format for `cancel` verification

Report concise but complete findings:

* List each tested route and whether it passed: running delegate, no-arg wait, wait suppression, duplicate cancel, unknown id, empty id, completed delegate, shell cancellation, unsupported non-shell tool, `sub_agent_id`, sibling isolation, slow duration, and inner-tool leak.
* Include exact unexpected errors or output.
* Mention any timing surprises, missed completion prompts, duplicate prompts, leaked inner completions, or ordering uncertainty.
* Confirm the `cancel` success output is only `Tool cancellation requested`; it is an async, best-effort request, not a delivery receipt for child cleanup.
* Include whether errors distinguish completed delegates from unknown ids.
* Include whether the `agent_start` placeholder made the target ID clear enough, and whether `self_agent_id` and `sub_agent_id` were present without redundant aliases.
* Include whether slow canceled delegates reported `duration_seconds`.
* Include whether the UI hid completion prompts that should be hidden, or whether that could not be directly verified.


### Message tool verification plan

Use this plan when asked to verify the `message` tool, especially in multi-agent scenarios. The goal is to prove that messages are routed correctly among the main agent, sub-agents, sibling sub-agents, the special `user` recipient, and completed or invalid recipients. Also verify timing, sender IDs, async delivery, payload escaping in hidden prompts, exact payload preservation in durable `AgentMessage` events, and error behavior.

Do not rely on memory. Give every sub-agent a self-contained prompt. A delegated agent starts with a clean context and does not know this skill, the parent conversation, or the IDs of other agents unless you include them in its prompt or later messages.

#### What to verify

Record all of these observations:

* Main agent to sub-agent delivery.
* Multiple messages to the same live sub-agent.
* Sub-agent to sibling sub-agent delivery.
* Sub-agent to the main agent using the main agent recipient ID.
* Sub-agent to `user` delivery, noting that this may be visible in the UI but may not appear as a model-visible inbound message to the main agent.
* Main agent to itself, after the main agent recipient ID is known.
* Delivery while a sub-agent is sleeping, backgrounded on a long tool, queued behind another tool call, or otherwise between model turns.
* Delivery order, or any reorderings, especially for parallel `message` calls.
* Sender IDs visible to recipients.
* Message payload preservation in durable events, and XML escaping in hidden prompts, for multiline content, blank lines, unicode, JSON-like text, backticks, and literal `<message>` tags inside the payload.
* Error for an unknown recipient ID.
* Error for a completed sub-agent recipient ID.
* Error for an empty message.
* `agent_start` and `wait` behavior around long-running sub-agents, including `duration_seconds` headers for slow delegates.

#### Phase 1: spawn two peer agents and use `user` for live reports

Start with two shared delegates. Name them Agent A and Agent B. They should stay alive long enough to receive delayed messages, log every inbound message, execute peer-send commands, and report to `user`. Reporting to `user` avoids needing the main agent recipient ID in the first phase.

Use this prompt for Agent A, replacing only the agent name where needed for Agent B:

```text
You are Agent A in a Tau message-tool verification test. Goal: verify cross-agent messaging behavior. You have a clean context; follow these instructions exactly.

Important:
- Incoming messages from the Tau `message` tool may appear as hidden prompts in your conversation. Treat every new prompt/message you see after starting as an inbound test message.
- Keep a full log of every inbound message you receive after this initial task prompt. Include exact text, apparent sender/recipient if visible, and when you noticed it.
- You may use only safe commands. Use short `sleep` commands only to stay alive and give the parent/peer time to send messages.
- If you receive a message containing `COMMAND: SEND_PEER`, parse `recipient_id={id}` and `text={text}`, then call the `message` tool to send exactly `{text}` to that recipient. Log the tool result.
- If you receive a message containing `COMMAND: REPORT`, send a `message` to `user` with your current full log.
- Do not finish early. Run four observation rounds.

Procedure:
1. Immediately send a message to `user` with exactly: `READY Agent A: started message-tool test`.
2. For rounds 1 through 4:
   a. Run `sleep 3` using the shell tool.
   b. After the sleep result, inspect any new inbound messages/prompts you have received.
   c. Execute any `COMMAND: SEND_PEER` instructions you have newly received.
   d. Send a message to `user` starting with `REPORT Agent A round {n}:` and include all newly observed inbound messages since the previous report and any message-tool actions/results. If none, say `none`.
3. Final answer: return `FINAL Agent A` plus your complete inbound-message log and all message-tool actions/results.

You are expected to receive messages from the parent and possibly from Agent B. Be precise and do not invent messages.
```

After the `agent_start` placeholder results return, note the caller `self_agent_id`, each `sub_agent_id`, and both agent_start tool call IDs. Use `sub_agent_id` as the message recipient. Send the first batch of messages in parallel:

```text
To Agent A:
- MAIN to A direct message 1. nonce=main-a-001. Please log exact text.
- MAIN to A direct message 2. nonce=main-a-002. Please log exact text.
- COMMAND: SEND_PEER recipient_id={agent_b_id} text=PEER A to B message from Agent A. nonce=peer-a-b-001. Please log exact text.
- COMMAND: REPORT from main to Agent A after initial sends. nonce=report-a-001.

To Agent B:
- MAIN to B direct message 1. nonce=main-b-001. Please log exact text.
- MAIN to B direct message 2. nonce=main-b-002. Please log exact text.
- COMMAND: SEND_PEER recipient_id={agent_a_id} text=PEER B to A message from Agent B. nonce=peer-b-a-001. Please log exact text.
- COMMAND: REPORT from main to Agent B after initial sends. nonce=report-b-001.
```

Sleep for about four seconds in the main agent, then send a delayed batch in parallel:

```text
To Agent A:
- MAIN to A delayed direct message 3. nonce=main-a-003. Please log exact text.
- COMMAND: SEND_PEER recipient_id={agent_b_id} text=PEER A to B delayed message from Agent A. nonce=peer-a-b-002. Please log exact text.
- COMMAND: REPORT from main to Agent A after delayed sends. nonce=report-a-002.

To Agent B:
- MAIN to B delayed direct message 3. nonce=main-b-003. Please log exact text.
- COMMAND: SEND_PEER recipient_id={agent_a_id} text=PEER B to A delayed message from Agent B. nonce=peer-b-a-002. Please log exact text.
- COMMAND: REPORT from main to Agent B after delayed sends. nonce=report-b-002.
```

Also send one message to a clearly invalid recipient such as `engineer_does_not_exist_message_validation`; expect a tool error with the unknown recipient ID and echoed message fields.

Wait for both delegates. In their final logs, verify that:

* The agent_start placeholder and final result expose `self_agent_id` and `sub_agent_id` without redundant aliases.
* Each agent saw the direct main-agent messages addressed to it.
* Each agent saw the peer message from the other agent.
* Each `COMMAND: SEND_PEER` caused exactly one peer `message` call with result `Message sent`.
* Delayed messages arrived even though the agents were already running.
* The visible sender ID for messages from the main agent is present and matches the `self_agent_id` from the agent_start result. Save that sender ID; it is the main agent recipient ID for the next phase.

After both delegates complete, try to send a post-completion message to each old `sub_agent_id`. Expect an error until completed-agent wakeup is implemented. Current behavior may report this the same way as an unknown recipient.

#### Phase 2: verify delivery to a delegate queued behind a backgrounded tool

Start one shared sub-agent with `agent_start` whose job is to create the message-delivery edge case where the sub-agent has a long backgrounded tool still actually running and a second not-yet-started tool queued behind it.

Use this prompt:

```text
You are a Tau message-tool verification sub-agent for queued-tool preemption. Goal: prove parent messages are delivered even when your next tool call would otherwise be queued behind a long backgrounded tool.

Procedure:
1. Start a long shell command: `sleep 30`.
2. After Tau returns the background placeholder for that shell call, immediately request another shell command: `sleep 5`.
3. If you receive any inbound `[tau-internal]` message from the parent, respond to `user` exactly: `QUEUED-TOOL MESSAGE RECEIVED nonce=queued-tool-message-001` plus the exact inbound message text and visible sender id.
4. After sending the user message, final answer exactly: `FINAL queued-tool message probe reported to user`.
5. If no inbound message arrives, final answer exactly: `UNEXPECTED queued-tool message missing`.

Do not invent messages. Do not finish before checking for the parent message.
```

After the agent_start placeholder returns, wait until the delegate has had enough time for the first `sleep 30` to background and for the second `sleep 5` request to be queued. In normal UI output this often looks like delegate progress with a running/backgrounded shell call and no response from the second shell yet.

Send a message to the delegate `sub_agent_id`:

```text
Parent queued-tool delivery probe. nonce=queued-tool-message-001. Reply via message to user when received.
```

Expected behavior:

* The message call returns `Message sent`.
* The delegate responds to `user` with `QUEUED-TOOL MESSAGE RECEIVED nonce=queued-tool-message-001` instead of remaining stuck behind the queued `sleep 5`.
* If event logs are available, verify that the `AgentMessage` was recorded, the not-yet-started queued tool call was terminalized with `ToolCancelled`, and a new `AgentPromptCreated` was emitted for the delegate message prompt.
* The long backgrounded `sleep 30` may still complete later in the delegate conversation. Its completion should not be delivered to the parent conversation or block the message response.

This scenario specifically protects the code path where `agent.message` delivery preempts queued-but-not-started tool calls behind an already-backgrounded exclusive tool. Without that behavior, the message can be received by the harness but never become a model-visible prompt for the sub-agent.

#### Phase 3: verify sub-agent to main-agent routing

Use the main agent recipient ID learned from Phase 1. Spawn two fresh shared sub-agents with `agent_start`, Agent C and Agent D. These agents should report back to the main agent recipient ID, not to `user`. This proves that parent-directed messages are delivered as model-visible `[tau-internal]` inbound messages to the main agent.

Use this prompt for Agent C, replacing only the agent name where needed for Agent D and filling `{main_agent_id}` with the ID learned in Phase 1:

```text
You are Agent C in a second Tau message-tool verification test. Parent/main agent recipient_id is `{main_agent_id}`. Goal: verify messages among parent, Agent C, and Agent D.

Rules:
- Incoming `message` tool messages may appear as hidden prompts. Log every inbound message you receive after this initial task prompt, with exact text and visible sender id.
- For every report, use the `message` tool to send to `recipient_id={main_agent_id}` (the parent/main agent), not `user`, unless the parent message fails. If it fails, log the failure and continue.
- If an inbound message contains `COMMAND: SEND_PEER recipient_id={id} text={text}`, send exactly `{text}` to `{id}` using the `message` tool and log the result.
- If an inbound message contains `COMMAND: REPORT_PARENT`, immediately message your current log to `{main_agent_id}`.
- Stay alive for three observation rounds using `sleep 2` each round. Do not finish early.

Procedure:
1. Send to `{main_agent_id}`: `READY Agent C to parent. nonce=ready-c-parent-001`.
2. Repeat three rounds: sleep 2 seconds; inspect new inbound messages; execute any SEND_PEER commands; message the parent with `REPORT Agent C round {n}:` plus new inbound messages and actions since previous report, or `none`.
3. Final answer: `FINAL Agent C` plus complete inbound log and all message-tool actions/results.
```

After the `agent_start` placeholders return, send this batch in parallel:

```text
To Agent C:
- MAIN to C direct message. nonce=main-c-001. Please log exact text and sender id.
- COMMAND: SEND_PEER recipient_id={agent_d_id} text=PEER C to D from Agent C. nonce=peer-c-d-001. Please log exact text.
- COMMAND: REPORT_PARENT nonce=report-c-parent-001.

To Agent D:
- MAIN to D direct message. nonce=main-d-001. Please log exact text and sender id.
- COMMAND: SEND_PEER recipient_id={agent_c_id} text=PEER D to C from Agent D. nonce=peer-d-c-001. Please log exact text.
- COMMAND: REPORT_PARENT nonce=report-d-parent-001.
```

The main agent should receive `[tau-internal]` inbound messages from each sub-agent. Record whether the sender ID in those inbound messages matches the sub-agent `sub_agent_id`. Sleep for about three seconds, then send one delayed direct message to each agent:

```text
To Agent C:
- MAIN to C delayed message. nonce=main-c-002. Please log exact text and sender id.

To Agent D:
- MAIN to D delayed message. nonce=main-d-002. Please log exact text and sender id.
```

Wait for both delegates. Verify that their final logs match the parent-visible reports already received by the main agent.

After both complete, again send post-completion messages to both old `sub_agent_id` values and expect errors until completed-agent wakeup is implemented.

#### Phase 4: verify self, content, and simple validation errors

After the main agent recipient ID is known, send a message from the main agent to itself. Expect a model-visible `[tau-internal]` inbound message whose sender is the same main agent ID and whose payload is exact.

Then send a multiline self-message like this:

```text
MULTILINE self content probe. nonce=self-main-002
line 2 unicode: café 🚀

line 4 xml-ish: <message>inner</message> & chars
line 5 code-ish: `backticks` and {"json":true}
```

Verify that blank lines, unicode, backticks, and JSON-like text remain readable, and that ampersands plus literal inner `<message>` tags are XML-escaped inside the delivered wrapper. If you inspect durable `AgentMessage` events, verify that the stored payload is still exact and unescaped.

Finally, call `message` with an empty string to a valid recipient. Expect a tool error such as `` `message` must not be empty ``. Also verify an unknown recipient error if it was not already checked in Phase 1.

#### Reporting format for `message` verification

Report concise but complete findings:

* List each tested route and whether it passed: main to child, child to child, child to parent, child to `user`, main to self, invalid recipient, completed recipient, empty payload, rich content payload.
* Include exact unexpected errors or output.
* Mention any timing surprises, missed messages, duplicate messages, or ordering uncertainty.
* Confirm the `message` success output is only `Message sent`; delivery is async, so no delivery receipt is expected.
* Include whether errors distinguish completed recipients from unknown recipients. Current behavior may use the same unknown-recipient error for both.
* Include whether parent recipient ID discovery was clear from `self_agent_id` or still had to be inferred from sub-agent logs.
* Include whether the delivered wrapper XML-escaped payloads containing literal `<message>` tags and ampersands.


### Agent watch tool verification plan

Use this plan when asked to verify the `agent_watch` tool. The goal is to prove that watch subscriptions deliver only the watched agent's final response notifications and received user-prompt notifications, that `agent_start` auto-watches its child, and that disabling a watch stops delivery without hiding errors.

Watch notifications must be distinguishable from explicit `message` tool deliveries in the model-visible prompt. Explicit messages use a “received a message from ...” wrapper with a `<message>` block. Watch response notifications use this exact shape:

```text
[tau-internal]: Watched agent engineer-aSSq emitted a response

<response>
The task is complete.
</response>
```

Watch prompt notifications use this exact shape:

```text
[tau-internal]: Watched agent engineer-aSSq received a user prompt

<prompt>
Please continue.
</prompt>
```

`agent_watch` must not forward tool-completion notices, background-tool wakeups, ordinary internal steering prompts, prompts delivered through the `message` tool, or any other internal prompt delivered to the watched agent. A watched agent may later emit a final response after processing such an input; the final response is the watchable event, not the internal input itself. A completed `agent_start` result is also a watchable final response of that started agent to the delegating watcher, even if the delegating watcher is itself a side agent.

If the watching agent repeats the same notification text in a commentary/final response, that is the agent echoing the notification, not a second watch delivery. When checking for duplicate delivery, count actual message events/results, not streamed echoes of text the model chose to repeat.

A watch notification that arrives while the watcher is blocked in `wait` may interrupt the wait with a `tau_internal: true` result saying new input is queued. Treat that as expected: it is the same active-wait interruption behavior used for ordinary agent messages.

Messages sent by a sub-agent to `user` are user-recipient messages. They are not model-visible to the parent agent unless the sub-agent also sends a message to the parent or includes the information in a watched response/final answer. Do not use `message` to `user` as the only proof that the parent observed a notification.

#### What to verify

Record all of these observations:

* `agent_start` automatically watches the returned `sub_agent_id` for the starting agent.
* Watch response notifications are model-visible as “Watched agent <id> emitted a response” with a `<response>` block, not as “received a message” with a `<message>` block.
* Watch prompt notifications are model-visible as “Watched agent <id> received a user prompt” with a `<prompt>` block.
* The watched agent's final responses arrive as async response notifications from the sub-agent to the watcher until disabled.
* A completed `agent_start` result from a watched child is reported as the child agent's final response to the watcher.
* Tool-completion notices and other internal/steering prompts delivered to the watched agent are not forwarded as watch prompt notifications.
* The `agent_start` final tool result contains metadata such as `self_agent_id` and `sub_agent_id`, but does not duplicate the sub-agent response text as `output`.
* `agent_watch({"agent_id": id, "enable": false})` disables notifications for that watcher.
* Re-enabling with `enable: true` restores notifications for later responses.
* Unknown, empty, or self `agent_id` values fail clearly. Stopped but known agents can still be watched or unwatched.
* Mid-turn tool-call responses do not notify early; notifications should correspond to final response semantics.
* Prompts delivered through the `message` tool do not produce watch-prompt notifications; they remain ordinary explicit-message deliveries to the watched agent.
* If a watched sub-agent errors or is canceled and the starter receives the watch message, the `agent_start` tool error may be the generic watch-delivered error. If the starter disabled watch or delivery failed, the original error, such as `Tool call canceled`, must remain visible in the tool error.

#### Suggested procedure

1. Start a sub-agent with `agent_start` whose prompt final-answers a nonce, for example `WATCH auto final nonce=watch-auto-001`. Do not ask the sub-agent to also `message` the user with the same nonce in this phase.
2. Confirm the starter receives an async `[tau-internal]` response notification from the `sub_agent_id` containing that nonce, using a “Watched agent <id> emitted a response” wrapper and a `<response>` block. It must not use the explicit-message “received a message from ...” wrapper or a `<message>` block. Avoid echoing the full notification text in commentary; summarize it when reporting.
3. Confirm the `agent_start` result exposes `self_agent_id` and `sub_agent_id` without returning the nonce as duplicated tool output.
4. Start a long-lived sub-agent. Disable watching it with `agent_watch({"agent_id":"<sub_agent_id>","enable":false})`, then cause or wait for a later response. Confirm no watch message is delivered to the starter for that response.
5. Re-enable the watch and cause another response. Confirm a watch message is delivered again.
6. Exercise validation: watch self, watch an empty id, watch an unknown id, and watch a stopped completed sub-agent. Unknown, empty, and self ids should error; the stopped but known sub-agent should be accepted. Record exact tool results or errors.
7. Deliver a real user prompt to a watched agent, or inspect event logs from a test that does so, and confirm the watcher receives exactly the “Watched agent <id> received a user prompt” wrapper with a `<prompt>` block.
8. Deliver an internal prompt to a watched agent if you can do so safely, or inspect event logs around a watched agent's background tool completion. Confirm no watch prompt notification is delivered for `[tau-internal] Tool call ... is complete.` or similar internal/steering text. If the watched agent later responds after processing that internal prompt, record that later response as a separate final-response notification.
9. Cancel a watched long-running `agent_start`. Confirm cancellation still reports a useful error. If watch delivery reached the starter, generic watch-delivered wording is acceptable; otherwise the original `Tool call canceled` must be preserved.

#### Reporting format for `agent_watch` verification

Report concise but complete findings:

* List each tested route and whether it passed: auto-watch, final response wrapper, user-prompt wrapper, internal/tool-completion non-forwarding, disable, re-enable, validation errors, cancellation/error fallback, and no duplicated `agent_start` output.
* Include exact notification text, tool results, and unexpected errors. Call out any watch notification that is formatted like an explicit `message` tool delivery.
* Mention duplicate notifications, missed notifications, premature mid-turn notifications, or unclear sender/recipient IDs. If a sub-agent was instructed to both `message` the user and final-answer with the same text, record those as two expected delivery paths rather than an `agent_watch` duplicate. If the watching agent repeats a received `[tau-internal]` notification in its own commentary/final response, record that as model echo unless event logs show multiple received deliveries. If a watched child produces a later response after an unfinished background tool completes, record it as a later child turn unless the same response event was delivered more than once.
* Include whether `wait` was interrupted by a watch notification while waiting; this is expected if it reports that new input is queued.
* Include whether `self_agent_id` and `sub_agent_id` made the watcher and watched IDs clear enough.


### Verification procedure

Create a scratch directory in `/tmp` for your experiments and always avoid dangerous or disruptive actions during testing.

For every tool thoroughly consider all corner cases, including ones which are not covered
in this document.

Report back:

* discrepancies between this document and actual usage,
* things that are wrong, confusing, inconsistent or unclear in both this document and actual tool output
* ideas for improvements both in the tool behavior and this document
