# SPEC-tau-harness-runtime-loop-guard: Runtime loop guard

The runtime loop guard is conservative and scoped to one loaded agent branch. It
tracks a bounded recent signature window and bounded breaker bookkeeping, injects
at most one internal pivot prompt for an obvious repeated cycle, and stops
automatic continuation with a mandatory notice if the same cycle persists after
the breaker was dispatched.

A provider `repetition_detected` response triggers the same lifecycle with a fixed
harness-authored reason. Provider error prose is display-only and never becomes
trusted pivot text.

New non-internal user input resets the guard even when queued. Successful
foreground or background tool results also reset detector and breaker history and
remove stale queued pivots, while retaining unresolved in-flight argument
signatures so a successful sibling in a multi-tool turn cannot make later failures
argument-insensitive. Non-linear branch or head movement invalidates all
branch-local guard state, including in-flight signatures and queued pivots.
