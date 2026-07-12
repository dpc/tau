# DESIGN-tau-harness-runtime-loop-guard: Runtime loop guard

Status: unconfirmed

The runtime loop guard is intentionally conservative and per-agent/branch only.
It tracks a bounded recent signature window and bounded breaker bookkeeping,
injects at most one internal pivot prompt for an obvious repeated cycle, and then
stops automatic continuation with a mandatory notice if the same cycle persists
after the breaker was dispatched.

Provider `repetition_detected` responses are treated as a loop-guard trigger with
a fixed harness-authored reason. The provider error is display-only; it is not
used as trusted pivot text.

New non-internal user input resets the guard even when the prompt is queued, and
successful foreground or background tool results reset it as clear progress.
Progress resets clear detector/breaker history and stale queued pivots but keep
unresolved in-flight tool-call argument signatures, so a successful sibling tool
in a multi-tool turn cannot make later failures argument-insensitive. Non-linear
branch/head moves invalidate the whole branch-local guard, including in-flight
signatures, and remove queued loop-guard pivots. Tests should exercise the
production response/tool/prompt wiring, not only private detection helpers: text
loops, repeated identical failures, different failure streaks, A/B/A/B suffixes,
queued user-input reset, success reset, branch invalidation, bounded breaker
state, argument-sensitive tool failures, and same-batch tool failures that must
receive the breaker before blocking.
