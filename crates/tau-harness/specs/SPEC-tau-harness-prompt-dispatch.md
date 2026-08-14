# SPEC-tau-harness-prompt-dispatch: Prompt Dispatch

## Record justification

Prompt dispatch combines model and role selection, effective tool policy,
provider definitions, authorization snapshots, dynamic context, prompt
fragments, and durable lifecycle events across harness modules and
provider/extension boundaries, so no local owner can state the complete contract.

Every accepted visible UI submission commits a content-free
`agent.user_interaction_recorded` fact, including an accepted queued prompt that
may later be recalled. Live untargeted shell output chooses among user agents by
the harness-local monotonic acceptance order; it never treats a derived
`meta.json` wall-clock value as routing authority. These rules preserve the
journal/checkpoint authority described by
[ARCH-tau-core](../../tau-core/specs/ARCH-tau-core.md).

## Prompt capability snapshot

Prompt capability conditionals use one turn-local snapshot. Tau resolves the
actual agent model, filters provider-supported and policy-effective tool specs,
and uses those same specs for provider definitions, authorization, fragments,
and template capabilities. Enabled configuration and Ready extension runtimes
are captured at that boundary. Later registration/restart changes affect only
later turns; raw capability context is not persisted. Non-tool extension side
queries are the intentional exception: provider definitions remain unchanged
for cache compatibility, while locally unauthorized tool capabilities and tool
fragments are empty.

The same concrete model snapshot supplies the parallel-call capability.
Templates advertise parallel execution only when the effective provider route
does; otherwise they state the one-call-per-response limit. Parsing,
persistence, and dispatch remain lossless if a provider violates a false claim
and returns multiple calls. Template data is sparse, render failures are
explicit and prevent provider dispatch, and capability state is not persisted
separately.

## Provider model registry

Provider model snapshots are flattened in lexicographically sorted source-id
order, with the last advertisement for an exact provider-qualified model id
winning both metadata and routing. Duplicate ids produce an ordinary warning
whose displayed id count and per-id text are bounded; this diagnostic does not
change winner selection.

Prompt submission may queue while configured extensions are still initializing.
After every queued extension connection and the global activation barrier settle,
an empty provider model registry rejects each runnable queued prompt exactly once
with `agent.prompt_rejected`, or with the correlated `agent.prompt_failed` terminal
for an accepted create-agent initial prompt. Both carry actionable configuration
guidance. `agent.prompt_rejected` is a harness-authored immutable must-pass live
event carrying only agent identity, message class, and the fixed guidance; it
contains no prompt text and never enters journals, replay, or provider context.
Queued prompts have no prompt id, so terminals map per agent in FIFO stream
order using the broadcast lifecycle alone. A submitted fact removes its matching
queue front while retaining `ctx_id` correlation, so a later correlated
`agent.prompt_failed` cannot consume newer work. Otherwise
`agent.prompt_failed` or `agent.prompt_rejected` consumes the matching
user/internal FIFO front.
No provider request is materialized for rejected work. A later model declaration
affects only new submissions, which use the ordinary selection and dispatch path.

## Tool prompt-surface policy

Extensions and providers publish metadata only: tools declare neutral `ToolTag`s
(such as `shell:edit:line`, `shell:edit:replace`, `shell:edit:apply_patch`,
`shell:exec:generic`, `shell:exec:shell_command`, and `shell:workdir`) and
providers publish model `ModelTag`s (such as `shell:chatgpt` and
`shell:tool-style:replace`). The harness owns all matching policy.

Before ordinary policy, the harness selects one shell edit surface from
`tool_policy.default_shell_tool_style`, one explicit `shell:tool-style:*` model
tag, or the legacy model default. Conflicting explicit style tags fail closed.
Tool enablement then starts from that selected surface and each extension's
`enabled_by_default`, then matching
harness `tool_policy.rules` run deterministically by `(priority, rule name)`,
with each rule applying `disable_tool_tags` before `enable_tool_tags`. Built-in
and user policy share the same evaluator; the built-in `builtin.chatgpt-shell`
rule disables `shell:*` for ChatGPT-tagged models and re-enables apply-patch,
shell-command, workdir, and directory-lock tools.

Role precedence is broad-to-specific and runs after global policy: optional
`tools` allow-list base, `disable_tool_tags`, `enable_tool_tags`,
`disable_tool_groups`, `enable_tool_groups`, `disable_tools`, then
`enable_tools`. This deliberately lets a role disable a broad family and
re-enable a narrower tag, group, or named tool.

Prompt dispatch snapshots the effective `ToolSpec` list for the selected prompt
model. Provider tool calls are validated against that prompt-owned snapshot, not
against mutable current role/model state after the user switches roles or models
mid-turn. Staged tool registration can never expand a prompt snapshot after it
was sent. Developer tool previews use the same model-aware effective selection
and render aliases as provider-visible names, so their output matches the tool
definitions delivered to the model.

Policy-exclusive registrations may share a model-visible alias, but snapshot
construction rejects an effective surface containing two enabled tools with the
same visible name rather than selecting by registry order. Prompt-owned
unavailable and near-name diagnostics derive from this snapshot as well.

Tool examples are registration metadata, not provider definitions. After a
failed call, the harness may append one bounded relevant example on the owning
agent branch and records that injection so retry loops do not receive repeated
scaffolding.

The extension-level `shell.workdir` fragment is prompt-visible only when the
effective snapshot contains a tool tagged `shell:workdir`. This keeps persistent
workdir guidance aligned with role, model, and global tool hiding. The existing
cross-source collision rule still emits at most one copy when several shell
instances publish the shared fragment. Workdir context contributions are also
filtered to the connections that own an effective `shell:workdir` tool, so
hiding one prefixed instance does not expose its path through another instance's
shared fragment.

The built-in available-agent-roles-for-`agent_start` catalog is prompt-visible
only when the effective snapshot contains `agent_start`. It applies across role
groups and renders the currently visible, available delegate role catalog from
per-agent context;
agents without `agent_start` omit the fragment from template data entirely.
Role visibility is presentation-only: hidden roles remain available to explicit
`agent_start` requests and retain their ordinary authorization and diagnostics.

Tools tagged `provider-content:image` survive effective-tool filtering only
when the selected route publishes image in both `input_modalities` and
`tool_result_modalities`. Role and global tool policy may narrow that result but
cannot force the tool onto a text-only route. The provider-side projection
independently fails closed so stale history or capability races never send image
bytes to an unaudited route.

Narrow schema-guided argument repair also uses the prompt-owned `ToolSpec`.
Repair runs only after pre-dispatch validation failure, applies a small fixed set
of mechanical conversions, revalidates before dispatch, and falls back to the
normal rejection diagnostics when repair is unsupported or still invalid. Repair
traces are bounded metadata for logs/UI, not prompt-surface examples.

The loop guard is runtime-only per loaded agent branch. It records compact recent
assistant/tool-failure signatures, injects one hidden pivot prompt for obvious
cycles, and surfaces a mandatory notice instead of continuing automatically if the
same cycle persists. New user prompts and successful tool results reset detector
history and remove pending loop-guard pivots, but preserve unresolved in-flight
tool-call argument signatures for sibling calls in the same turn. Branch/head
moves invalidate the whole guard, including in-flight signatures, and remove
pending loop-guard pivots.

Provider-side `repetition_detected` final responses feed this same lifecycle with
a fixed harness-authored reason: first occurrence queues the pivot, recurrence
after that pivot stops automatic continuation. Provider error text is displayed
but is not trusted as model-visible guard instruction.

Named context-size alerts compare a successful finished ordinary inference's
provider-reported input-token usage with each enabled effective role threshold.
The effective alert map is prompt-owned, so a role change during inference cannot
alter response-time policy. Usage must strictly exceed the threshold. Failed,
canceled, stale, duplicate, reactive-recovery, standalone-compaction, and
inline-compacting responses do not create alert work. Each crossed alert queues
its configured text as an internal prompt after the current response; tool calls
finish before the prompt continues the turn. When delivery commits, the existing
`agent.prompt_submitted` or `agent.prompt_steered` fact carries
`internal_kind=context_size_alert`. Within one daemon lifetime,
an alert fires once while usage remains above its threshold and becomes eligible
again after usage falls to or below the threshold or context accounting resets.
An accounting reset also removes any still-queued alert prompts so a compaction
cannot be followed by stale advice from the old usage climb.
Crossing and queued-delivery state are intentionally runtime-only, like other
advisory prompt scheduling: cold replay neither synthesizes missed alert work nor
persists one-shot suppression, and a later successful response re-evaluates
restored usage. Disabled alerts remain inherited config but never inject prompts.
The tagged delivery fact itself remains ordinary durable transcript history and
replays at its original position; no second notice or synthetic replay entry is
created. This implements
[SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md).

Harness-generated active and passive background-tool completion prompts carry
`internal_kind=background_tool_completion` when they reach model context. The
tag changes only UI classification: activation, prompt delivery, wait
suppression, retained-result consumption, and replay position remain unchanged.

## Prompt dispatch lifecycle split

Prompt dispatch first completes the foreground append of a lightweight, harness-authored
`agent.prompt_started` materialization fact. That fact includes the provider
operation, captured `ModelParams`, and the owning durable outer-turn id for
ordinary inference, and must uniquely match one unresolved durable inference
checkpoint or standalone-compaction start. Its one-shot live post-commit continuation then
publishes the full transient `agent.prompt_created` provider work request.
Provider delivery does not wait for background journal sync; a crash can leave a
delivered request without that fact in the recovered prefix.
Providers consume `agent.prompt_created`; UIs and side-effect observers should
subscribe to `agent.prompt_started` so materialized prompt context and tool
schemas are not sent over UI/control channels unnecessarily. Cold replay folds
prompt-start facts for audit and generation state but never recreates full work
or includes prompt starts in subscriber catch-up.

Immediately before selected-provider delivery, the harness requires the same
session generation and loaded runtime
incarnation, the exact unresolved owner and compact fact, unchanged
agent/prompt/model/operation identity, and the route captured from that model.
Any mismatch fails closed. Persisted full `agent.prompt_created` records are
unsupported legacy data; operators must discard or reset those journals rather
than relying on decoding compatibility or migration.
This authority chain is governed by
[SPEC-compact-prompt-materialization-authority](../../../specs/SPEC-compact-prompt-materialization-authority.md).

Typed image bytes in provider tool results are never generic UI traffic. Live
`provider.tool_result` delivery excludes UI clients; they receive the separate
payload-free `tool.result_display` event. Historical UI replay projects the durable
provider event back to that same typed display event. Debug and TRACE prompt
projections that serialize a prompt as JSON recursively remove image buffers
before serialization. The built-in provider's ordinary TRACE projection instead
emits fixed content-free structural metadata without serializing prompt content.
Debug JSONL represents full prompt work as a bounded content-free count summary
rather than serializing prompt structure or content.

All subscriber broadcasts and historical replay project typed image buffers out
of provider tool results, live full-prompt contexts, compaction windows, and
structurally possible provider-response items. Only durable agent storage and
the selected provider's point-to-point prompt receive canonical bytes. Pending
tool-call state retains whether the exact registered tool carried
`provider-content:image`; a function tool without that tag cannot return media.
Semantic validation and prospective encoded-record validation run before result
deduplication or generic success publication.

## Canonical external-message facts

Committed `message.*` facts project as ordinary escaped context after universal
field validation. Prompt assembly never adds reply routes or actionable
capabilities; those remain private to the publishing extension. Replay rebuilds
payload-free activation for uncovered activating facts without rebuilding
extension-local authority.

Harness-owned agent-message facts use the analogous placement/checkpoint
mechanics but remain a distinct typed domain. Each directional durable
occurrence remains its only payload authority. Ordinary outbound `Message`
occurrences are omitted from sender provider context; local inbound messages
render the escaped stable-sender `<tau_internal>` envelope from the typed fact,
and live activation uses only a sequence-keyed payload-free wake. Replay
reassembles context and rebuilds one uncovered payload-free wake per activating
occurrence. See
[SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md).

## Interactive user prompt provider projection

The harness stamps `HumanUi` on accepted visible UI prompts for existing agents,
new-agent initial prompts, and queued prompts later committed as steering facts.
Submitted and steered facts keep the accepted effective text raw and carry that
required typed provenance through the derived transcript. Prompt assembly alone
projects each such entry as one fieldless `<user>...</user>` user-role text item,
replacing only exact `</user>` collisions. Replay follows the same source-based path.

Successful existing-agent skill commands expand against that agent's frozen
initialization snapshot before acceptance. New-agent initial skill commands queue
raw and expand only after the same agent's initialization finalizes, before the
durable prompt fact is submitted. The canonical expanded `<skill>` block remains
raw in the fact while the complete expansion is preserved inside the provider
`<user>` body. Internal, injected, extension,
external-message, agent-message, and watch domains retain their existing
projections. UI/history/tree anchors, activation, queueing, and wake behavior do
not consume the provider wrapper. See
[SPEC-interactive-user-prompt-envelope](../../../specs/SPEC-interactive-user-prompt-envelope.md).
Exact-sentinel framing and the conditional model-visible provenance rule follow
[SPEC-exact-sentinel-prompt-envelopes](../../../specs/SPEC-exact-sentinel-prompt-envelopes.md).

Prompt dispatch is blocked while the target agent has a pending discovery
initialization. Prompt assembly reads model-visible skills and tool lookup from
the frozen snapshot and materializes the reducer's latest AGENTS.md
initialization side-state once as a user context block outside transcript nodes.
New-agent eager initial prompts remain in the harness-owned preprocessing queue
until that exact initialization freezes, including the interval before a
per-agent wait is installed. Strict system-prompt rendering and render preflight
run only after every required per-agent context provider reports readiness; no
missing-context fallback weakens template validation. Once ready, that accepted
initial prompt remains ahead of later replay activations and message wakes, so
the first provider turn preserves acceptance order.
See
[SPEC-session-discovery-declarations-and-readiness](../../../specs/SPEC-session-discovery-declarations-and-readiness.md).

Compaction-triggered dispatch and continuation refine [SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md); that record owns their transaction, checkpoint, replay, and model-correlation behavior.

## Prompt capability trust boundary

Prompt capability data is sparse. Tool names are only the model-visible,
policy-authorized, provider-supported names advertised for that prompt; internal
aliases and registered-but-hidden tools are not exposed. It contains no
commands, secrets, failure text, or disabled extension catalog. Extension
`active` means protocol Ready, not feature health or sandboxing.
