# ARCH-tau-ext-utils: tau-ext-utils architecture

`tau-ext-utils` is a first-party utility extension. It owns the model-visible
`timer` tool in the `timer` group and an opt-in `papercut` diagnostic reporter.
The reporter is declared only when its configured instance enables
`papercut.enable`; ordinary global and role tool policy still controls its
effective visibility.

`papercut` accepts only a bounded report string and uses the existing
per-instance `ExtensionDataScope::User` `AppendFile` RPC to append one
newline-terminated v1 JSONL record to its owned `papercuts.jsonl` relative
filename. The shared `PapercutRecord` type defines this v1 contract for the
reporter and the developer CLI. The harness resolves it to
`<tau-state>/ext/<configured-std-utils-instance>/papercuts.jsonl` and
serializes User-scope appends across harness processes sharing that state root
and instance. The record attributes the report from the routed tool caller and
current live `session.started` fact; it never accepts model-supplied
attribution. It is best-effort diagnostic data, not timer or journal state:
memory-only storage denial, quota, RPC failure, and the accepted rare lifecycle
rollover mismatch return a no-retry outcome without interrupting the primary
task. Ephemeral sessions append to the same durable file. Papercut calls are
live-only and never replay-append. Existing per-session papercut files remain
historical artifacts and are not migrated.

The approved narrow operator exception from task 149f lets `tau dev papercut
list` and `clear` read only the normal `std-utils` instance's canonical file.
It never
selects arbitrary extension data or exposes other extension payloads. Both
commands use the ordinary Tau state root or their explicit `--state-dir`;
absent standard-instance storage is an empty history and clear is a successful
zero-record no-op. They take the same extension-directory lock as `AppendFile`.
Clear validates then deletes the complete file while holding that lock, so
records appended before its lock boundary are removed and appenders released
after the boundary create a new preserved file. Malformed, unsupported,
oversized, symlinked, non-regular, or unrenderable records fail closed and
remain intact.
This exception uses the explicit semantics approval in
[GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

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
fire time beyond the current wall clock. Relative schedules retain their
existing initial-delay and optional fixed-interval behavior. Daily wall-clock
schedules instead carry one exact `HH:MM` time: `utc=true` uses UTC, while the
default resolves the running host's local timezone. They use the first matching
instant strictly after scheduling, skip nonexistent spring-forward times, and
choose the earlier instant for repeated fall-back times. Multiple overdue daily
occurrences coalesce into one prompt with an exact count. Calendar-distance and
timezone-transition work bounds that count independently of elapsed days.
Replayed schedule and prompt facts recalculate future instants using the running
host configuration. The canonical timer prompt timestamp is the occurrence
floor, so a backward clock move cannot repeat an already prompt-recorded daily
occurrence.

The runtime reads one Jiff system-timezone snapshot per refresh cycle and uses
that same snapshot for every local timer calculation in the cycle. Jiff discovers
platform host configuration, including Unix `TZ` overrides and regular or linked
`/etc/localtime` TZif rules, and caches discovery process-wide for approximately
five minutes. While local timers are active, the timer runtime polls that source
on a 60-second monotonic cadence; host configuration changes therefore affect
timers after Jiff's cache expires. Already-due deadlines fire before
reinterpretation; replaying agents wait for their boundary; transient lookup
failure retains accepted restored state for a later refresh. No timezone name or
database version enters durable timer state. Multiple daily times remain
independent timers rather than one grouped or cron-like schedule.

For Clank ticket `3y67`, the user approved these interface, local-time, DST,
missed-fire, and recovery semantics, including registration at `tool.started`,
due-before-reinterpretation ordering, prompt timestamps as the backward-clock
progress floor, retained unresolved state after transient discovery failure, and
Jiff's approximately five-minute system-discovery cache. This satisfies
[GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

After live timer mutations and successful timer replay, the extension declares
`timer_scheduled` exactly when an agent's reconstructed timer map is nonempty.
One-shot fire removes presence; periodic fire retains it. The extension emits
replacement declarations after map mutations, while the harness clears source,
agent-unload, and session-rollover contributions. Scheduled timers are not
modeled as active tool calls.

Session lifecycle is explicit: live `session.started` and `session.shutdown`
clear all active timer state, and `session.agent_unloaded` makes that agent's
timers dormant until a later successful replay boundary. Schedule requests
reject duplicate active ids instead of acting as implicit updates. The default
safety floor is 10 seconds for one-shot delays and 60 seconds for fixed recurring
intervals. Existing per-agent and per-session bounds also cover daily timers.

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
