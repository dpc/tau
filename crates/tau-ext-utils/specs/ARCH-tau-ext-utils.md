# ARCH-tau-ext-utils: tau-ext-utils architecture

`tau-ext-utils` is a first-party utility extension. It owns the model-visible
`timer` tool in the `timer` group and an opt-in `papercut` diagnostic reporter.
The reporter is declared only when its configured instance enables
`papercut.enable`; ordinary global and role tool policy still controls its
effective visibility.

`papercut` accepts only a bounded report string and uses the existing
session-scoped extension-data `AppendFile` RPC to append one newline-terminated
v1 JSONL record to its owned `papercuts.jsonl` relative filename. The record
attributes the report from the routed tool caller and current live
`session.started` fact; it never accepts model-supplied attribution. It is
best-effort diagnostic data, not timer or journal state: memory-only/ephemeral
storage denial, quota, RPC failure, and the accepted rare lifecycle rollover
mismatch return a no-retry outcome without interrupting the primary task.
Papercut calls are live-only and never replay-append.

The extension uses deferred startup so it can validate this closed
per-instance configuration before dynamically declaring its tools and prompt
fragments. This preserves normal configured prefix scoping and avoids exposing
papercut or its prompt when disabled.

Timers are session-scoped operational state, not a separate durable database. The
extension reconstructs active timers by folding catch-up input:

1. replayed `tool.started` for `timer` records the original call arguments by
   `call_id`;
2. replayed successful `tool.result` / `provider.tool_result` applies the
   schedule/cancel mutation from those original arguments;
3. replayed timer-created `agent.prompt_submitted` and `agent.prompt_steered`
   events with `ctx_id` values of the form `timer:<timer_id>:<count>` remove
   one-shot timers or advance periodic timers, including prompts queued while an
   agent was busy;
4. non-replay `agent.replay_complete` gates firing, so overdue restored timers do
   not submit prompts until the owning agent's catch-up has reached its boundary.

Timer wakeups use the narrow `extension.internal_prompt_submit_request`, which
has no user-message class, is sent explicitly with `Emit.persist=false`, and
carries typed `timer` activation provenance. The optional provenance does not
change configured-extension authority, replay, or recovery; it only lets the
harness copy an exact content-free classification into
`agent.activation_queued`. The harness remains the only component that
publishes `agent.prompt_submitted`; the extension never forges transcript prompt facts.
See
[SPEC-internal-prompt-submit-requests](../../../specs/SPEC-internal-prompt-submit-requests.md).
Periodic timers coalesce downtime into one internal prompt and advance the next
fire time beyond the current wall clock.

Session lifecycle is explicit: live `session.started` and `session.shutdown` clear all active timer state, and `session.agent_unloaded` makes that agent's timers dormant until a later successful replay boundary. Schedule requests reject duplicate active ids instead of acting as implicit updates. The default safety floor is 10 seconds for one-shot delays and 60 seconds for recurring intervals.

## Timer tool display

Timer tool result/error display metadata is derived from validated `TimerAction`
values for valid calls, so compact UI lines can show action and timing details
without re-parsing untrusted strings. If argument parsing fails, display falls
back only to whitelisted action labels (`schedule`, `cancel`, `list`) plus
sanitized timer ids and bounded numeric fields. Unknown actions and invalid timer
ids are not echoed into `ToolUseState.args`. Successful list displays report the
bounded number of returned timers as the standard match counter. Schedule and
successful cancel need no additional chip because their validated action
arguments already identify the mutation; a static `not active` chip distinguishes
an idempotent cancel from one that removed a timer without exposing reminder text.
