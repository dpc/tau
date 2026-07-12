# DESIGN-tau-harness-activating-input-wait: Activating-input wait lifecycle

Status: confirmed, 2026-07-12, user

`wait({"any_input":true})` suspends one foreground tool call until canonical
inference-activating input is queued for that agent. It remains within the same
outer running agent turn and tool round; no suspended lifecycle state or watch
edge is introduced. Wakeup is target-scoped and content-free and does not
consume the input or an unsuppressed background completion. Registration is
runtime-only: live reconnect preserves it, while cold restore repairs the
unresolved tool instead of recreating indefinite suspension.
