# DESIGN-tau-harness-activating-input-wait: Activating-input wait lifecycle

Status: confirmed, 2026-07-12, user

`wait({"timeout_minutes":N})` suspends one foreground tool call until canonical
inference-activating input is queued for that agent or its monotonic deadline
expires. Positive integer minutes are required and values above 60 are silently
treated as 60. It remains within the same outer running agent turn and tool
round; no suspended lifecycle state, idle notification, or watch edge is
introduced. Wakeup is target-scoped and content-free and does not consume the
input or an unsuppressed background completion. Registration is runtime-only:
live reconnect preserves it, while cold restore repairs the unresolved tool
instead of recreating suspension.

## Rationale

Only the input form can park without concrete owned work, so bounding it avoids
indefinite suspension without weakening exact-id or next-background result
collection. Explicit minute units avoid a hidden conversion contract; accepting
and capping larger values is forgiving while enforcing the product's one-hour
bound. Keeping deadline arbitration on the harness event loop preserves the
existing single-writer, exactly-once input/cancellation ordering. A waiting tool
is still part of a running outer turn, so inventing an idle/watch transition
would conflict with the lifecycle model and could itself activate watchers.

Detailed behavior is specified by
[SPEC-tau-harness-activating-input-wait](SPEC-tau-harness-activating-input-wait.md).
