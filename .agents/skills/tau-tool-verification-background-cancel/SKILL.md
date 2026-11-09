---
name: tau-tool-verification-background-cancel
description: >
  Use this skill when verifying Tau background tool execution, wait, cancel, or
  agent_start interruption, including result consumption, completion prompt
  suppression, races, delegate interruption, cancellation isolation, and event
  logs.
advertise: false
---

# Tau Tool Verification Background Cancel

Load `tau-tool-verification` first for the shared output structure, escaping,
line handling, tool-description, availability, and reporting guidelines.
This skill supplies the focused verification guidance for this tool group.

### Background tools and `wait`

The placeholder carries `status: running-in-background` and `call_id: <canonical-id>`. Use that ID for exact wait or cancel; do not invent an ID or assume native correlation is model-readable. Successful `cancel` returns empty text and accepts cancellation without promising completion. Final cancellation source carries `status: canceled` in its body, along with available output or an unavailable-capture diagnostic.

Accepted incremental updates and final source appear once in the registered `tool_output` envelope with `call_id`, `terminal`, and nonzero `omitted_bytes` attributes. Tool name and sequence remain canonical metadata, not envelope attributes. `wait` returns a synchronization receipt with `cause` and `outcome` (`completed`, `error`, or `canceled`); the original call's receipt contains only `outcome`. Receipts do not copy source, and there is no separate completion/source-copy notice or retrieval instruction. `tool_output` pages retained accepted output; it cannot recover omitted raw source or expired files.

Verify source by call ID and observed context order, including the terminal
source after all accepted updates. When canonical events are available, also
check their sequence numbers. A shell terminal can contain only a suffix; look for earlier
content in prior updates rather than treating it as missing completion output.
Pair these checks with the file-shell segment and bounded-retention probes. A receipt's
`outcome: completed` means tool execution completed, not that the process
exited zero: assert a nonzero status in source and a completed receipt. For
plural waits, check receipts in request order without expecting copies of the
source inside them.

Verify that a real shell prints a sentinel and stays alive: the sentinel must arrive
after 15 seconds idle or 30 seconds of continuing output, before completion,
and the agent must be able to cancel in response. Silence must not produce
updates, size pressure must not flush early, and accepted chunks must remain
ordered before final source. Live inference must not be cancelled by output.
For every wait mode, `complete_only: true` must mask already queued and newly
arriving nonterminal tool wakeups for that agent while retaining source context.
Unrelated input and actual completion must still wake it.

Use `wait({"tool_call_id": "..."})` for a specific completion. `wait({})` consumes the oldest unconsumed background completion in the current conversation; if none is complete and an owned call is running, it waits for the next completion. Otherwise it returns an error. It must not consume parent, child, or sibling completions. Exact waits may target running calls. Source and synchronization remain separate.

`wait({"timeout_minutes": N})` is the separate activating-input form. `N` must
be a positive integer and is silently clamped to the configured bounds (one
through 1,440 minutes by default). It returns
`input_available: true` when accepted input wins or `timed_out: true` when the
monotonic deadline wins, without consuming input or background results. Exact-id
and bare background waits remain unbounded. Registration and expiry stay inside
the running outer turn and must not emit an idle/watch lifecycle edge.
After three consecutive input-wait timeouts without a status report or
substantive tool admission, the third result also carries one `advice` string
suggesting `status(waiting)` and event-driven wakeups. Later timeouts in that
no-progress run omit it, and current Waiting suppresses it.

Exact-id and bare waits arbitrate completions against activating input.
A wait may consume its matching completion before that completion's own
queued wakeup preempts it. A different unsuppressed completion wakeup,
watch notification, or direct message is ordinary activating input and may
interrupt the wait even when the requested call is already complete. The
successful interruption result uses `interrupted: <typed-input>` and `cause: <actual-source-id>`. Mode is UI-only; do not expect an `internal`/`reason`/`mode` header block. After the input drains, retrying the exact wait must still deliver the receipt once.

Do not infer an exact-wait result from a delegate's prose. Confirm the actual
tool request contained `tool_call_id`. A request containing
`timeout_minutes` instead exercises activating-input wait and correctly returns
`input_available: true`.

Current background timing: most backgroundable tools background after about 2
seconds, and `wait` itself never backgrounds. `agent_start` currently finishes
instantly after creating the sub-agent and returns only `sub_agent_id`; the sub-agent's later turns and final answers are delivered to
the starter by `agent_watch`, which `agent_start` enables automatically. Older
Tau versions treated `agent_start` itself as a backgrounded tool call; if you see
that older behavior, report it as version/config-specific rather than assuming
the current watch-based semantics are broken.

Because `agent_start` now finishes instantly, slow delegate work is normally
observed through watch notifications and the delegate's own background tool
results, not through a slow `agent_start` result with `duration_seconds`.

When asked to verify the `agent_start` tool, also verify delayed `message` delivery to a live delegated sub-agent whose own tool turn is parked behind a backgrounded tool. This is a delegate-specific regression path, not only a `message` tool test. Use a delegate prompt that first runs `sleep 30`, then after the background placeholder requests a second shell command `sleep 5`, and asks it to report to the parent agent ID if it receives a parent message. After the first shell backgrounds and the second shell request is queued, send `message` to the delegate `sub_agent_id` with a nonce. Expected: a result with only `msg_id: <actual-message-id>` and no payload; the queued `sleep 5` is terminalized internally, and the delegate promptly reports receiving the nonce instead of staying stuck until `sleep 30` finishes. If event logs are available, confirm `AgentMessage`, `ToolCancelled` for the not-yet-started queued call, and a new `AgentPromptCreated` for the delegate message prompt. Treat omission of this scenario as incomplete `agent_start` verification.

Also verify the active-`wait` variant of the same scenario. Use a delegate prompt that starts a long backgroundable tool, then calls `wait` on that tool call ID before it completes. While the delegate is blocked in `wait`, send `message` to the delegate `sub_agent_id` with a nonce. Expected: a result with only `msg_id: <actual-message-id>` and no payload; the delegate's `wait` returns promptly with the typed `interrupted` and `cause` interruption headers, and the delegate receives the hidden message prompt without waiting for the original background tool to complete. If event logs are available, confirm the wait `ToolResult` appears before the message-driven follow-up `AgentPromptCreated`.

Because `agent_start` enables a persistent watch, these message-delivery probes can produce later watch notifications if the child leaves an earlier background tool running. For example, after an active-`wait` interruption, the original sleep may complete later, deliver background completion in the child, and the child may answer something like `Received.`. That is not a duplicate watch notification by itself; it is a later child turn caused by the delayed inner completion. If the verifier no longer wants notifications from that child after the success nonce, explicitly call `agent_watch({"agent_id":"<sub_agent_id>","enable":false})`.

The first successful `wait` consumes a queued background completion and returns its receipt. Later waits for the same id should fail with an already-consumed error. Parallel duplicate waits on the same id race; at most one should receive the receipt, and the rest should fail. At most one *installed* no-arg waiter may target the same next completion: with no queued completion and one running call, sibling bare waits must produce one registration and one clear duplicate/in-progress error. If two completions are already queued, two sibling bare waits may consume them in completion order; that does not install duplicate waiters. The exact race error depends on timing, but each completion must be consumed at most once.


### Background tool `cancel`

`cancel` requires `tool_call_id` and never backgrounds. It supports running
backgrounded tool calls such as slow shell commands. Older Tau versions also
supported canceling a backgrounded `agent_start` call; current `agent_start`
finishes instantly and therefore usually does not expose a cancellable
`agent_start` tool-call id. A successful cancel request returns empty text, emits a harness notice event containing `tool call
cancellation request`, and targets only the requested tool call. Cancellation is
async and best effort: the success result only means Tau accepted the request,
not that the child process or agent has already stopped. A canceled shell call
should close through a successful `wait` receipt with `outcome: canceled`.
Timing and captured diagnostics belong to source output, not the receipt. The
process must not keep running to normal `exit_code: 0` completion.
For an admitted ext-shell model call, cancellation processed before the
extension's effect-start transition prevents process spawn or file mutation and
produces one cancelled terminal. If effect start wins first, cancellation keeps
the active process/search behavior and cannot roll back effects already started.

Calling `cancel` for an unknown, completed, or unsupported tool call should return a tool error. Unknown ids should be distinguished from already-completed ids. Calling it twice for the same target should return a tool error like `Tool call already canceled`.

Verify the placeholder and `tool_output` source are visible, and `wait` returns a completion receipt once and only once. Check suppression of queued completion wakeups by an active matching wait or by consumption before delivery; a wakeup already delivered cannot be retracted. Do not require a separate model-visible completion notice.


### Cancel tool verification plan

Use this plan when asked to verify the `cancel` tool, especially around
background shell calls, `wait`, duplicate requests, and any still-supported
background `agent_start` behavior. Current `agent_start` normally finishes
instantly, so delegate-cancel phases are conditional: run them only if the live
`agent_start` result exposes a background tool-call id.

Do not rely on memory. Give every sub-agent a self-contained prompt. A delegated agent starts with a clean context and does not know this skill, the parent conversation, or other agent IDs unless provided in its prompt or later messages. It receives its parent sender ID in the harness bootstrap.

Create a scratch directory in `/tmp`, such as `/tmp/tau-cancel-verification.*`, before running shell probes. Keep all sleeps short except where a background transition or leak check requires a longer wait.

#### What to verify

Record all of these observations:

* For historical background `agent_start` behavior, record the placeholder fields and background agent_start tool call ID.
* If canceling `agent_start` is supported, `cancel` must be called with the agent_start `tool_call_id`, not the `sub_agent_id`.
* A successful cancel returns empty text and does not background.
* The harness emits a `harness.notice` event containing `tool call cancellation request` if event logs are available.
* If delegate cancellation is supported, the canceled delegate produces a background error that `wait` can collect.
* `wait({"tool_call_id": id})` returns a successful receipt with `outcome: canceled` once and only once for current tools. Preserve the version-specific error expectations below for legacy delegate calls.
* `wait({})` can collect a canceled completion with `cause` and `outcome: canceled`.
* Waiting before the delegate completes suppresses its later queued completion wakeup. Waiting after a wakeup was delivered is still valid and does not retract it.
* Duplicate cancel requests race cleanly: one succeeds, later or parallel ones fail with `Tool call already canceled` or another clear duplicate error.
* Canceling an unknown id, unsupported running tool id, empty id, or `sub_agent_id` returns a tool error. If legacy/background `agent_start` is present, a completed agent_start id also returns a clear already-done error.
* If delegate cancellation is supported, canceling one delegate does not cancel a sibling delegate.
* Canceling a long-running shell call works and does not let the command complete normally.
* Timing metadata belongs to canonical source output, not the completion receipt. A few seconds of timing overhead is normal and not worth reporting by itself.
* If delegate cancellation is supported, a canceled delegate does not leak completions from its own in-flight or backgrounded inner tool calls into the parent conversation.
* Check UI handling of internal completion wakeups against the current UI settings.

#### Phase 1: running delegate happy path (legacy/conditional)

Run this phase only when `agent_start` returns a background placeholder with a
tool-call id. In current watch-based `agent_start` sessions it is not applicable.

Start a shared sub-agent with `agent_start` with this prompt:

```text
You are a Tau cancel-tool verification sub-agent. Goal: stay alive until the
parent cancels this agent_start call.

Procedure:
1. Wait for an inbound `BOOTSTRAP parent_id={main_agent_id}` message.
2. Send a message to that parent ID exactly: `READY cancel-ready-probe: entering long sleep`.
3. Run `sleep 60` using the shell tool.
4. If you are not canceled, final answer exactly: `UNEXPECTED cancel-ready-probe completed without cancellation`.

Do not do anything else.
```

After the legacy placeholder result returns, record `self_agent_id`,
`sub_agent_id`, and the agent_start tool call ID. Send
`BOOTSTRAP parent_id={self_agent_id}` to `sub_agent_id`, wait for the `READY`
report, then call `cancel` with that
agent_start tool call ID. Expect the foreground result to be empty.

Then wait for the same tool call ID. Expect a background tool error like:

```text
error: Tool call canceled
self_agent_id: ...
sub_agent_id: ...
```

Call `wait` for the same ID again. Expect an already-consumed error. Call `cancel` for the same ID again. Expect `Tool call already canceled`.

#### Phase 2: no-arg wait and wait suppression (legacy/conditional)

Run this phase only when `agent_start` returns a background tool-call id.

Start another long-sleeping sub-agent with `agent_start`. Cancel it, then call `wait({})`. Check the cancellation source and a synchronization receipt with `cause` and `outcome: canceled`.

Start a third long-sleeping delegate. Call `cancel` and `wait({"tool_call_id": id})` in parallel or as close together as possible. Expect `wait` to return the cancellation receipt. Check suppression of a pending wakeup when wait is active; a wakeup delivered before wait becomes active is not a suppression failure.

#### Phase 3: invalid targets and duplicate requests

Verify each error case independently:

* `cancel({"tool_call_id": ""})` returns `` `tool_call_id` must not be empty ``.
* A clearly unknown call ID returns `Unknown tool call id` and echoes `tool_call_id`.
* If legacy/background `agent_start` is present, a completed agent_start ID returns `Tool call is already done`.
* A `sub_agent_id` returns `Unknown tool call id`; this proves the tool wants the agent_start call ID.
* If legacy/background `agent_start` is present, two parallel `cancel` calls for the same live delegate produce one success and one duplicate-cancel error.

For the completed-agent_start case, run this only when legacy/background
`agent_start` ids are exposed. Spawn a sub-agent with `agent_start` that
immediately returns:

```text
You are a Tau cancel-tool verification sub-agent. Return immediately with exactly: `FINAL cancel-completed-probe normal completion`.
```

Wait until completion arrives, then try to cancel the legacy
background `agent_start` id. After that, call `wait` and verify the normal final
answer is still available once.

#### Phase 4: running shell cancellation

Start a shell command long enough to background, such as `sleep 20`. When the shell placeholder gives a tool call ID, call `cancel` for that ID. Expect the foreground result to be empty.

Then call `wait` for the shell call. Expect a successful receipt with `outcome: canceled`. Check the canonical source for captured diagnostics and timing, not the receipt. If the command ran longer than about 5 seconds, verify that the source includes a `duration_seconds` header. If `cancel` rejects the shell call as not cancellable, or if `wait` later returns `outcome: completed` with normal `exit_code: 0` completion in the source, record this as a discrepancy because shell cancellation is expected to work.

#### Phase 5: target isolation (legacy/conditional)

Run this phase only when `agent_start` returns background tool-call ids.

Start two sub-agents with `agent_start` in parallel. The target should sleep for a long time. The survivor should sleep briefly and return `FINAL cancel-survivor unaffected`.

Cancel only the target delegate. Then wait for both IDs. Expect:

* Target: `error: Tool call canceled`.
* Survivor: normal final answer.

Any sibling cancellation, missing survivor result, or cross-talk between IDs is a bug.

#### Phase 6: slow delegate cancellation and duration (legacy/conditional)

Run this phase only when `agent_start` returns a background tool-call id.

Start a long-sleeping sub-agent with `agent_start`. Let it run long enough to cross the delegate duration threshold, usually about 6 seconds. Cancel it and wait for the result. Expect the canceled agent_start result to include `duration_seconds` with an approximate whole-second value.

Do not require an exact duration. Internal overhead and scheduling can add a few seconds of jitter; do not report small delays by themselves.

#### Phase 7: nested and inner-tool leak check (legacy/conditional)

Run this phase only when delegate cancellation is supported by a background
`agent_start` tool-call id.

This phase is important. A canceled delegate can have its own foreground or background tool call in flight. Canceling the delegate must not leave an orphaned inner tool completion that later wakes the parent conversation.

Start a shared sub-agent with `agent_start` with this prompt:

```text
You are a Tau cancel-tool verification sub-agent for inner-tool leak testing. Goal: start an inner tool call, then be canceled by the parent.

Procedure:
1. Run `sleep 12` using the shell tool.
2. If you are not canceled, final answer exactly: `UNEXPECTED cancel-inner-tool-leak completed without cancellation`.

Do not send messages. Do not do anything else.
```

Let the delegate run long enough for the inner shell call to background, usually about 3 seconds. Then cancel the delegate and wait for the agent_start result. Expect `error: Tool call canceled`.

After the delegate cancel result is consumed, watch for stray completion source or wakeups for any other tool call ID, especially the inner shell call. If stray completion source or a wakeup appears, call `wait` for that ID and record the full result. Treat this as a leak unless there is a clear documented reason it belongs to the parent conversation.

If no stray completion appears before the inner `sleep 12` would have finished, record that no leak was observed. This check caught a prior manual discrepancy where a canceled delegate's inner `sleep` later produced a parent-visible completion.

#### Optional event-log checks

If you have direct access to harness event logs, verify:

* Successful cancel emitted `harness.notice` with `tool call cancellation request`.
* The canceled delegate emitted `ToolBackgroundError` with `Tool call canceled`.
* No `AgentPromptSteered` or queued pending prompt remains for canceled nested delegate completions.
* Each completion is consumed once and is unavailable to later `wait` calls. Canonical source remains separate from consumption.

#### Reporting format for `cancel` verification

Report concise but complete findings:

* List each tested route and whether it passed: shell cancellation, unknown id, empty id, unsupported non-shell tool, and `sub_agent_id`; when legacy/background `agent_start` ids are available, also report running delegate, no-arg wait, wait suppression, duplicate cancel, completed delegate, sibling isolation, slow delegate duration, and inner-tool leak.
* Include exact unexpected errors or output.
* Mention any timing surprises, missed completion source, duplicate source or wakeups, leaked inner completions, or ordering uncertainty.
* Confirm the `cancel` success output is empty; it is an async, best-effort request, not a delivery receipt for child cleanup.
* When legacy/background `agent_start` ids are available, include whether errors distinguish completed delegates from unknown ids.
* Include whether current `agent_start` results make `sub_agent_id` clear enough and whether the delegate bootstrap identifies the parent; when legacy/background `agent_start` ids are available, also include whether the placeholder made the cancellable target ID clear enough.
* When legacy/background `agent_start` ids are available, include whether slow canceled delegates reported `duration_seconds`.
* Include whether the UI handled internal completion wakeups as configured, or whether that could not be directly verified.
