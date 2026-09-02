# SPEC-tau-harness-prompt-dispatch: Prompt Dispatch

Prompt, tool-terminal, and compaction facts that authorize dispatch or
continuation first complete semantic-persistence admission. Rejection retains or
rolls back the owning continuation and cannot expose publication or repeat an
uncertain external effect. Accepted asynchronous suffixes remain live facts
before the sole worker makes them durable.

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

When that accepted prompt is authenticated HumanUI, non-internal, and
inference-activating, it may request supersession of the target's exact ordinary
dispatch-uncertain owner after FIFO insertion and prompt lifecycle observation.
One per-agent runtime owner coalesces later prompts and retains the exact
old-prompt Stale event and semantic parent through interception and admission
retry. Any exact provider terminal or higher-priority retained completion,
tool/background, compaction, output-length, or outer-finish owner resolves
first. Only canonical Stale commit and fold releases uncertainty and lets FIFO
dispatch mint a new prompt id; Tau never resends the old request. Transaction-
owned compaction uncertainty is excluded.

## Prompt capability snapshot

Prompt capability conditionals use one turn-local materialized surface. Tau
resolves the actual agent model, filters provider-supported and policy-effective
ordinary tool specs, and separately selects provider-hosted definitions. The
same surface supplies provider definitions, authorization, fragments, and
template capabilities. Enabled configuration and Ready extension runtimes are
captured at that boundary. Later registration/restart or model declarations
affect only later turns; raw capability context is not persisted.

Standalone compaction retains the parent's ordinary and hosted definition
surface for request-cache equivalence while using provider `tool_choice: none`.
Non-tool extension side queries instead set `tool_choice: none` and suppress
both ordinary and hosted logical web definitions before provider delivery.
They also hide tool instructions and locally reject any provider violation.
Authenticated cross-harness peer entrypoints retain extension provenance but use
the ordinary tool-capable surface immediately. The peer message remains
untrusted request content rather than user authority; the configured target role,
tool policy, and registered tool routes remain the capability boundary. A later
authenticated human UI prompt records user originator authority independently of
payload message class or claimed originator.

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

Before ordinary policy, the harness selects one shell edit implementation from
`tool_policy.default_shell_tool_style`, one explicit `shell:tool-style:*` model
tag, or its default. Untagged non-ChatGPT models use the exact-text `replace`
implementation by default; `shell:chatgpt` models use the Codex `apply_patch`
surface. The `edit`, `replace`, and `codex` selectors continue to choose the
line-coordinate, exact-text, and apply-patch implementations respectively.
Both non-Codex implementations are provider-visible as `edit`, while extension
routing retains their distinct internal names. For exact-text calls, the
provider definition and canonical request/result use `edit`, while routed
`tool.started` and ext-shell reports use internal `replace`. Conflicting explicit
style tags fail closed.
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

`agents.web_tools` compiles the logical `web_search` and `web_fetch`
capabilities before visible-name collision checks and system-prompt rendering.
Named candidates merge across role/profile layers and are considered in
`(priority, name)` order. A `model_provider` search candidate is eligible only
when the exact selected route advertises the requested hosted controls. A
`tool` candidate must be an authorized Function tool with the expected visible
alias and shared protocol operation/enforcement tags. The compiler exposes
exactly one implementation per logical capability. Native search suppresses
only its configured ordinary fallback by internal name; unrelated alias
collisions still fail. Selection is frozen for retry and continuation, with no
native-to-external fallback.

The central domain policy becomes provider-side filters only when the exact
hosted route advertises them. For ordinary tools, a nonempty policy requires an
advertised enforcement tag. The harness freezes external fetch domains in
prompt runtime state and adds them to the durable `tool.started`
`invocation_policy`; model arguments and `tool.request` cannot author or relax
it. Peer-originated requests without a prompt snapshot receive the default
empty invocation policy.

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
The same effective snapshot extends only the cloned provider-facing
`agent_start` description with its sorted visible, available role names. This
out-of-band discovery hint remains present when a custom system-prompt template
omits prompt fragments; no visible names produces the unchanged base
description.

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

Named context-size alerts compare a successful, accepted ordinary inference's
provider-reported input-token usage with each enabled effective role threshold.
The effective alert map is prompt-owned, so a role change during inference cannot
alter response-time policy. Usage must strictly exceed the threshold. Failed,
canceled, stale, duplicate, reactive-recovery, standalone-compaction, and
inline-compacting responses do not create alert work.

An `after_response` alert queues its configured text after the response and, when
tool calls are present, after those calls finish; delivery continues the current
turn. An `outer_turn_finished` alert is evaluated against the accepted terminal
response and logical finishing status, then wakes a fresh internal-prompt turn
only after the current outer turn commits its finish. It never makes cancellation,
error, challenged-response, or other non-response finishes notice-eligible. When
either delivery commits, the existing
`agent.prompt_submitted` or `agent.prompt_steered` fact carries
`internal_kind=context_size_alert`. Within one daemon lifetime,
an alert at either lifecycle point fires once while usage remains above its
threshold and becomes eligible
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

Prompt-injected notification admission remains separate from provider-trigger
readiness. A natural user prompt, timer prompt, or tool continuation
opportunistically folds every already-admitted selected-branch notification at
its materialization cut, even when a notification's own deadline is later. A
notification that becomes ready during active inference or ordinary foreground
tool work never starts concurrent inference; it waits for the next safe
continuation. Provider-unavailable failure applies only after trigger readiness.

## Prompt dispatch lifecycle split

Prompt dispatch first completes bounded persistence admission and the in-memory
fold of a lightweight, harness-authored `agent.prompt_started` materialization
fact. That fact includes the provider
operation, captured `ModelParams`, and the owning durable outer-turn id for
ordinary inference, and must uniquely match one unresolved durable inference
checkpoint or standalone-compaction start. Its one-shot live post-commit continuation then
publishes the full transient `agent.prompt_created` provider work request.
Provider delivery does not wait for journal or checkpoint filesystem I/O; a
crash can leave a delivered request without that fact in the recovered prefix.
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
[SPEC-provider-prompt-materialization-authority](../../../specs/SPEC-provider-prompt-materialization-authority.md).

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

## Output-length successor

An eligible reasoning-only `Length` response reserves exactly one new prompt id
for the current consecutive reasoning-only run inside the active outer turn.
Durable order is:

`provider.response_finished(continuation_planned)` →
`agent.prompt_steered(output_length_continuation)` →
`agent.inference_dispatch_started(output_length_continuation owner)` →
`agent.prompt_started` → transient `agent.prompt_created`.

Only each fact's write-complete callback may create the next fact. The internal
steer is a user-role internal instruction with harness-internal submission
source, a trusted span covering the full UTF-8 text, and exactly:

`The prior response reached the output-token limit before producing an answer or tool request. Stop extending the analysis and take the next concrete step.`

New successors and replay accept only this exact text.

It grants no model, route, branch, tool, or compaction authority. The successor
uses the captured provider-qualified model and activation cut. Branch movement,
cancellation, or a missing logical model route cannot redirect the reservation.

A successful canonical selected-branch ordinary inference response rearms the
spent budget at response commit when it has stop `tool_calls` or the accepted
`end_turn`-with-calls shape and at least one canonical tool call. Dispatch and
execution outcome do not affect that boundary. Length-truncated calls,
empty-call stops, tool results, prompt counts, compaction, and off-branch
responses do not rearm it. Multiple plans may occur in one outer turn; each
keeps `ordinal=1, limit=1` and its source and successor prompt ids identify its
lineage. Replay derives the latest run from these existing durable facts.

A context-rejected reserved successor carries recovery disposition only. The
exact successful reactive-compaction descendant keeps the same output-length
owner and budget; another `Length` terminalizes it.

If branch selection leaves the committed plan, the harness appends the exact
steer, owner, empty pre-start `Failed` terminal, and owed finish beneath the
dormant original branch. It restores the selected sibling after each write,
never emits successor `agent.prompt_started` or provider work, and gives the
sibling one UI notice without model-visible reasoning or failure state.
This synthetic failure repair is authorized only before durable successor
`agent.prompt_started`. If branch selection changes after prompt-start, Tau does
not mint a competing pre-start failure; the already-dispatched owner remains the
sole terminal authority.
