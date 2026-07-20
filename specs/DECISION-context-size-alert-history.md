# DECISION-context-size-alert-history: Context-size alert delivery is visible durable history

Authority: confirmed, 2026-07-20, dpc

When a named context-size alert reaches an agent, its existing durable prompt
fact carries `internal_kind: Option<InternalPromptKind>`. Both fresh-turn
`agent.prompt_submitted` delivery and in-flight `agent.prompt_steered` delivery
use `Some(InternalPromptKind::ContextSizeAlert)`, serialized as
`context_size_alert`, for this source. A UI renders only that tagged source at
the fact's existing journal position as
`[tau-internal]: <exact configured text>`. Live delivery, late attachment, and
cold resume all present the same fact in original agent-history order.
`internal_kind: None`, a missing legacy field, and other internal prompts retain
their hidden behavior. Visibility is never inferred from `ctx_id` or prompt
text. Interceptors may observe the fact but cannot add or remove its tag or
rewrite tagged alert text.

Alert threshold crossing, pending queue state, and one-shot suppression remain
runtime-only. Tau does not synthesize a history entry for an alert that crossed
but was cleared before delivery, and it does not add a separate notice or
duplicate transcript fact.

## Rationale

The committed prompt fact is the exact point when alert text enters
model-visible agent context. Tagging and rendering that fact avoids a transient
UI print that disappears on attach or resume, a second event that can drift from
transcript order, and text parsing that could misclassify unrelated internal
prompts.

## Tradeoffs

The durable `agent.prompt_submitted` and `agent.prompt_steered` schemas gain the
optional internal-prompt tag. Older records lack this marker and keep their
historical hidden rendering. The alert text becomes visible in UI history even
though it remains classified as internal rather than user or external input.

This decision is required by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md),
and refines
[SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md),
and
[SPEC-tau-harness-prompt-dispatch](../crates/tau-harness/specs/SPEC-tau-harness-prompt-dispatch.md).
