# DESIGN-tau-harness-compaction-activation-binding: Durable compaction and activation binding

Status: confirmed, 2026-07-12, user

New inference checkpoints own a complete provider-qualified model, inference
operation, and activation-cut tuple together with their prompt, transcript
watermark, and optional compaction transaction. Post-commit materialization,
parameters, tools, accounting, and point-to-point provider routing use that
ownership rather than mutable model selection. A transaction checkpoint
must match its standalone start's model and cut; if the exact route disappears
before commit-time delivery, providers are excluded and the owner is durably
terminalized without remote send.

Standalone compaction binds durable Started and Compacted facts with the exact
transaction, cut, suffix end, pre-minted prompt id, provider-qualified model,
and standalone operation. New boundaries require all six fields: the
transaction resolves its Started fact; cut, prompt id, model, and operation
match Started; operation is standalone; `suffix_end` equals the boundary
parent; and cut is its ancestor. Legacy boundaries have all six absent. The
provider connection id is runtime-only and must not be persisted.

Every provisional activation or standalone-compaction cut is normalized to a
provider-valid closed prefix before it is committed. In the validated transcript
shape, a tool-calling assistant node is followed by one whole terminal-results
node, so a cut at that assistant retreats to its parent. Threshold selection,
pending-message wake selection, ordinary inference checkpoints, reactive
overflow, and explicit blocked recovery all use this same normalization.
`resume_through` remains the complete owed watermark, preserving the exact
assistant call plus function/custom/mixed results in the suffix.

A blocked retry through `/compact` or authorized `agent_compact` supersedes the
failed transaction at its normalized cut. New input does not clear the block,
and `/cancel` does not abandon it. This preserves exact activation ownership
until the explicit successor succeeds. Recovery refuses a selected head that no
longer descends from both the normalized failed cut and the failed resume
watermark.

Canonical submitted, injected, and steered facts contain a harness-owned,
default-false `inference_activation` marker. Typed harness provenance marks
passive background/restore context false and actual activators true; neither
prompt text, peers, nor interceptors control it. Completed checkpoints consume
true activations through their branch head. A checkpoint without a durable
terminal response restores as dispatch-uncertain and is not resent.

The cross-crate test strategy fixes these boundaries at their owning layer:
`tau-proto` covers missing/false/true serde behavior; `tau-core` covers the
all-six group, exact mismatches, duplicate/unknown outcomes, and legacy
boundaries; and `tau-harness` covers Started-before-dispatch and terminal
correlation, interception/peer ownership, typed passive replay, crash restart,
checkpoint ranges, and dispatch uncertainty.
Closed-prefix coverage adds core normalization and ancestor/same-branch
supersession validation; harness threshold, reactive, warm/cold, cross-agent,
queued-input, cancellation, and replay ownership scenarios; a strict in-process
provider/tool vertical slice that rejects unbalanced compact input; and ChatGPT
request serialization assertions for exact function/custom call-output pairs.
Restored post-compaction continuation coverage includes captured-route success,
staggered unrelated discovery, discovery-complete absence, explicit model
removal, warm resume, mutable role/model drift, sanitized terminal visibility,
and replay exactly-once behavior.
Pre-Ready provider model updates are coalesced per provider before activation.
The final staged snapshot determines captured-model presence; earlier staged
presence followed by final omission is an authoritative removal, while absence
throughout remains unresolved until discovery completes. Awaiting-checkpoint
runtime state carries provider-qualified model, inference operation, and
activation cut as one complete ownership value.
