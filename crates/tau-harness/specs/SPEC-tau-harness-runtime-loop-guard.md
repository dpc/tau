# SPEC-tau-harness-runtime-loop-guard: Runtime loop guard

## Record justification

Runtime loop prevention spans assistant-response classification, tool terminal
publication, branch-local state, prompt scheduling, provider terminal handling,
and durable delivery facts, so no single implementation artifact can own the
complete behavioral contract.

The runtime loop guard is conservative and scoped to one loaded agent branch. It
tracks a bounded recent signature window and bounded breaker bookkeeping, injects
at most one internal pivot prompt for an obvious repeated cycle, and stops
automatic continuation with a mandatory notice if the same cycle persists after
the breaker was dispatched.

A provider `repetition_detected` response triggers the same lifecycle with a fixed
harness-authored reason. Provider error prose is display-only and never becomes
trusted pivot text.

New non-internal user input resets the guard even when queued. Ordinary
successful foreground or background tool results also reset detector and breaker
history and remove stale queued pivots, while retaining unresolved in-flight
argument signatures so a successful sibling in a multi-tool turn cannot make
later failures argument-insensitive. Successful self-compaction instead counts
as another no-progress cycle. When that repeated cycle exhausts the guard, the
committed terminal remains visible but its already-owned post-commit standalone
continuation is suppressed. Non-linear branch or head movement invalidates all
branch-local guard state, including in-flight signatures and queued pivots.

The separate repeated-wait guard counts consecutive activating-input waits that
time out without a substantive tool admission or a new status report. The third
timeout adds one model-visible advisory to call `status` with `waiting`, finish
the current turn (status alone does not finish it), and rely on a message or
trigger for an event-driven wake. It does not reject, shorten, or otherwise
change the wait. The advisory is one-shot for that no-progress run, and an
already reported `waiting` phase suppresses it.
