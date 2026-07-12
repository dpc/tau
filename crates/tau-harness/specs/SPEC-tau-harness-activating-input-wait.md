# SPEC-tau-harness-activating-input-wait: Activating Input Wait

## Activating-input waits

`wait({"any_input":true})` is a runtime-only, target-scoped suspension inside an
existing tool round. It completes only after canonical input for that agent has
passed its normal acceptance boundary and is queued with inference activation:
user or extension prompts, internal/timer prompts, agent or authenticated
transport messages, later model-visible watch notifications, loop-guard pivots,
and unsuppressed activating background-completion prompts. Passive restore or
background notices, progress, replay-only traffic, foreground sibling results,
and input for another agent do not wake it.

Queueing and waiter registration both execute on the harness event-loop thread.
Starting a wait first checks the same per-agent pending-activation predicate;
otherwise it registers one waiter. Canonical queueing first stores the input and
then removes that agent's waiter. This level-triggered invariant covers both
queue-before-register and register-before-queue without polling. Wakeup neither
dequeues input nor consumes a background completion, and the content-free result
only promises that input is available.

Input-wait notification is distinct from background-result arbitration. Exact
and bare waits consume a matching already-completed result before its queued
completion prompt can preempt them. A different unsuppressed completion prompt
is ordinary activating input and interrupts them in either queue/register order;
an exact or bare waiter that consumed/suppressed the completion produces no
prompt to wake an input waiter.

The foreground wait keeps `AgentTurnState::ToolsRunning`; it introduces no
suspended lifecycle state or idle/running watch edge. Live UI reconnect does not
affect it. Unload, cancellation, rollover, or shutdown drops it, with event-loop
ordering deciding races exactly once. Cold restore does not recreate a waiter:
the unresolved foreground tool follows standard interrupted-tool repair while
durably accepted activation remains available through normal replay.

## Authorization boundary

Activating-input waits are scheduling, not a new input authority. They wake only
after canonical accepted input is committed or queued for that exact target
agent, using harness-owned inference-activation classification. Raw extension or
transport traffic, rejected or replay-only envelopes, asserted sender labels,
and another agent's input cannot wake them. The wait result is bounded and
content-free (`input_available: true`): external payload, sender, provenance, and
reply capability remain solely in the canonical typed envelope delivered by
normal prompt machinery and are never rewrapped as harness-authored tool output.
Wait registration is runtime-only harness state: cancellation, target unload,
session rollover, and shutdown remove it, and cold recovery uses ordinary
unresolved-tool repair rather than reviving stale scheduling authority.
