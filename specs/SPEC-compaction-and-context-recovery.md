# SPEC-compaction-and-context-recovery: Compaction and context recovery

## Record justification

Compaction and recovery span core transcript cuts and boundaries, harness transaction/checkpoint ownership, provider execution, typed message activation, context accounting, cold restore, and CLI/model authority, so no component-local documentation can state the complete closed-prefix and recovery contract.

The user explicitly approved the durable failure, retry, suppression, replay,
and successor semantics added for ticket `9dvw`, satisfying
[GATE-persistence-and-extension-interface-change-approval](GATE-persistence-and-extension-interface-change-approval.md).

Typed image tool results are indivisible members of their existing closed
call/result round. Durable canonical bytes replay through normal inference and
standalone compaction using the same provider converter. Approximate context
accounting includes encoded bytes and conservative 32-by-32 image patches;
provider adapters separately enforce aggregate canonical-image and generated
data-URL request bounds. A compacted replacement may summarize an old image
away like any other input fact. This behavior is confirmed by
[GATE-typed-image-tool-results](GATE-typed-image-tool-results.md).

The default Tau-owned summary fallback for Chat Completions, OpenRouter, and
public Responses models uses cache-aligned ordinary-prefix compaction.
Provider-native ChatGPT/Codex compaction remains preferred and unchanged. The
fallback derives conservative limits and a proactive threshold from the model
context window; an explicit `local_summary_compaction` profile fully overrides
those defaults.

The compaction request lowers the selected immutable cut exactly as ordinary
inference does: the same system prompt, tool definitions, ordered typed history,
images, raw tool-call arguments, route/model fields, and configured cache
controls. It appends one harness-authored user message last, inside
`<tau_internal>`, which asks for a continuation summary and forbids tool calls.
The ordinary prefix is not canonicalized, stripped, or rewritten, so a provider
may reuse the prefix warmed by ordinary inference. Cache reuse remains a
provider behavior, not a Tau guarantee.

Any returned tool call or other semantic output rejects the compaction and is
never executed, including when assistant text is also present. Exactly one
nonempty bounded final assistant text is accepted. Provider reasoning may be
separately bounded and discarded; attempted tool calls, reasoning, and opaque
replay items do not become semantic history. The accepted text becomes exactly
one synthetic user-role replacement message, with no wrapper, escaping,
deterministic supplement, or second rewrite. The private local narrative
envelope carries that validated text only across the extension-to-harness seam.
The immutable cut, transaction, suffix preservation, exact-once commit, and
live/cold replay contracts remain unchanged: a committed summary is replayed
after restart without another model call. Ordinary opted-in provider debug
capture policy also applies to local compaction and remains non-semantic
observability data.

## Recovery authority

The CLI `:compact` command is human/UI authority. The model-callable `compact`
tool is a separate, enabled-by-default self-compaction authority, while
`agent_compact` is an independently opted-in authority over any *other loaded
agent in the same harness session*. Enabling either tool never enables the
other. Agent ancestry, watches, message capability, role names, and knowledge
of an id do not grant compaction authority; conversely, the binding
`agent_compact` capability does not require ancestry. Unknown, unloaded,
stopped, and cross-session targets share a non-enumerating error.

Accepted model requests are harness-owned durable facts correlated to the
originating prompt and tool call. Each carries a unique bounded request ID and
immutable caller, target, model, tool-call, accepted-head, and prompt
correlation. Provider and extension input cannot select a cut, model, or caller,
and exactly one durable start or pre-start failure may claim an accepted
request. Projection exposes waiting, started (including transaction outcome),
and failed state so every crash window can be repaired without resending
ambiguous provider work or duplicating a background completion.
Every newly selected compact cut is a provider-valid closed prefix. A tool-calling assistant
response and the one terminal results node that closes its complete function,
custom, or mixed parallel round are indivisible at this boundary. A provisional
pre-activation cut at the assistant response retreats to its parent, retaining
the exact call-and-result round in the suffix rather than consuming the owed
activation.
Immutable historical failed starts with an open prefix remain replay-valid only
so explicit recovery can supersede them at a normalized closed ancestor without
rewriting durable history.
Context-limit diagnostics are sanitized at the harness trust boundary. They
retain only bounded numeric/model/operation categories under normal session
event retention and never prompt content, provider bodies, credentials,
headers, account identifiers, or raw error prose. Provider-authored telemetry
is overwritten, and observations cannot automatically alter safety thresholds.

## Reactive context-overflow recovery

An ordinary inference that receives a canonical, no-output context-window rejection may authorize exactly one standalone compaction when the captured model still matches, advertises standalone support, and role policy permits compaction. The terminal response and recovery disposition commit before a uniquely correlated compaction start. Compaction dispatch and continuation reuse the existing durable transaction machinery. Standalone-compaction overflow, a post-compaction inference overflow, a second overflow, and ambiguous dispatch are terminal rather than recursive. Partial output, cancellation, unsupported policy, legacy checkpoints, and branch/model mismatch never authorize recovery. Replay resumes an unclaimed planned recovery once, treats an interrupted compact dispatch as blocked, and retains the existing dispatch-uncertain rule after inference dispatch.

## Manual compaction

UI `:compact` may preempt a busy target only when its sole remaining foreground
call is the same still-installed harness-owned exact, bare, or activating-input
`wait`. The harness exclusively claims that waiter, commits one canonical
cancelled terminal with null provider output, and starts ordinary manual
compaction only after that terminal closes the complete tool round. The
compaction provider may therefore read the closed cancelled round; later
inference sees the replacement window rather than an unresolved structural
wait.

Event-loop order chooses races. A wait result, activating input, timeout, or
ordinary cancellation that settles first retains the normal busy rejection. A
compaction claim that wins excludes those wait terminals, coalesces repeated
requests, and runs before queued activation dispatch. Cancellation append
failure restores ordinary wait arbitration and never starts compaction.
Teardown clears the transient request and never starts compaction. Once
cancellation commits, provider failure, route failure, transaction
cancellation, or blocked recovery never resurrects the wait.

Harness-owned `compact` and `agent_compact` requests persist acceptance before
acknowledgement. Self requests start only from the complete tool-round
continuation gate, preserving tool-call/result adjacency. Cross-agent requests
may start for idle or explicitly blocked loaded targets and never queue behind
busy or dispatch-uncertain work. The target-scoped standalone transaction is
the provider-work authority. A self request's accepted placeholder never starts
ordinary inference. Its durable terminal produces one compact-specific internal
continuation containing bounded status and request, call, and transaction
correlation; this consumes the background terminal, suppresses generic completion
delivery, and makes the original call unavailable to `wait`. Success continues
from the replacement window. Failure or cancellation attempts the same
error-bearing continuation and otherwise leaves the transaction durably blocked
and visible. Replay delivers a committed but undelivered terminal once and never
resends ambiguous compactor work or repeats committed delivery. An explicit
recovery creates a successor outcome and delivery rather than rewriting history.
Cross-agent `agent_compact` remains asynchronous and waitable.
Private provider diagnostics for standalone ChatGPT compaction follow the
ordinary Responses WebSocket capture contract and never become journal, event,
UI, or recovery authority.
The model-callable path accepts work only when the exact captured
provider-qualified model supports standalone compaction and its route exists.
It has no inline fallback. Provider terminal errors, including context-window
rejection during standalone compaction, produce one terminal transaction
failure and are not retried indefinitely. Standalone compaction uses the shared
transient-failure classifier and jittered Fibonacci scheduler with a named
five-attempt policy, including the first attempt. Deterministic failures,
including context-window exhaustion, are terminal immediately. Ordinary
inference deliberately retains its unbounded transient-retry policy.
The Codex adapter serializes the first v2 compaction probe for a route/account
generation. A compaction-specific request rejection removes standalone
capability for that generation; explicit tools and automatic recovery share the
downgrade. Credential/account generation changes reject stale observations and
restore one fresh probe. Negative capability evidence is not persisted.
An explicit `:compact` or authorized `agent_compact` request may recover a
terminally failed standalone transaction. Its successor may preserve the
failed cut or retreat it along the same ancestor path to obtain a closed
provider prefix, but may never advance or cross branches and may not drop a
retained resume obligation. Its resume watermark must remain on the original
owed branch. Navigating away refuses supersession of that failure but permits a
fresh independent explicit transaction on the selected branch. The failed
durable start's provider-qualified model and selected-branch ancestry are the
authority after warm continuation and cold replay.
Ordinary activating input remains usable but does not clear or implicitly retry
the failure. The matching failed model and branch suppress automatic threshold,
policy, continuation, and reactive recovery so queued ingress cannot create a
hidden repeat loop. Explicit UI, self, and cross-agent requests remain recovery
authorities. Model or branch drift permits a fresh independent transaction
without rewriting the failed history; returning to the failed model and branch
restores suppression. Only a successful explicit successor clears the matching
durable failure chain.
When a failed transaction carries `resume_through`, that watermark must be an
ancestor of the successor watermark; a sibling branch with superficial
nonemptiness is invalid. Only a failed standalone transaction may be
superseded, duplicate transaction outcomes are rejected, and the latest
validated successor determines blocked, successful, and
continuation-checkpoint recovery.
Context-window rejection records may include sanitized, harness-owned
`context_limit_telemetry`. Correlation is the enclosing prompt plus exact
provider-qualified model and operation. Projection is accepted only from a
same-model usage baseline; missing, zero, stale/model-changed, or contradictory
inputs produce absent values or `insufficient_evidence`. The exact serialized
transcript delta remains separately attributable byte provenance, not projection
input. Projection independently counts byte-free JSON structure, canonical
encoded-image bytes, and rounded-up 32-by-32 image patches. It may corroborate
provider usage but never establishes a categorical observation without nonzero
provider-token evidence. The record makes hidden overhead and advertised-limit drift observable
but does not feed back into automatic calibration.
Calibration is explicit only: operators may change normal persisted model/role
configuration (including bounded thresholds), and resetting that configuration
removes the calibration. Tau never learns or mutates provider limits from a
rejection. The durable evidence records the active threshold and the closed
chosen action (`terminal` or `reactive_compaction_planned`).

## Tool and telemetry trust boundary

Internal registration places `compact` in `compaction` and `agent_compact` in
`cross_agent_compaction`; self-compaction is enabled by default and cross-agent
compaction is disabled by default. Runtime caller identity comes from committed
tool ownership, never arguments. Cross-agent possession authorizes any other
loaded agent, but self, unavailable, stopped, unloaded, and cross-session targets
are rejected without state enumeration. Watches, messages, ancestry, and
automatic-compaction role settings are not substitutes for explicit tool
presence.

The harness, not providers, owns the durable context-limit diagnostic attached
to terminal responses. Provider-supplied values are discarded. The schema is
content-free and bounded to one record per rejected prompt: model id, operation,
optional token counts/window, optional exact serialized transcript-growth bytes,
reserve, active threshold, closed policy/eligibility/action, and a closed
observation enum. Exact growth and projection are derived independently; each
field is absent only when its own serialization or checked aggregation is
unavailable. A categorical observation requires a positive advertised limit and
nonzero provider input usage; the transcript projection only corroborates or
makes contradictory evidence insufficient. Raw evidence remains present even
when the bounded observation is insufficient. Raw prompts, errors, response
bodies, headers, accounts, and endpoints are excluded.
Normal session/event retention applies; watcher snapshots do not duplicate this
record. Evidence never automatically lowers limits or thresholds.

## Recovery eligibility boundary

Reactive context recovery never trusts provider prose or a provider-authored recovery decision. Eligibility uses the closed failure category, an empty output set, harness-owned prompt operation/model routing, durable activation cut, advertised model capability, and role policy. Watchers receive only the existing sanitized `recovering_context` state; prompt bodies and raw provider errors are not included.

Provider-supplied recovery disposition remains observable on the committed report
but is unconditionally cleared and rederived by the terminal pipeline before
canonical publication. Any accepted streamed semantic output makes the response
recovery-ineligible. Cancellation durably terminalizes an active reactive
compaction transaction. The report boundary is specified by
[SPEC-provider-execution-reports-and-canonical-facts](SPEC-provider-execution-reports-and-canonical-facts.md).

## Harness dispatch refinement

Standalone compaction is transaction-driven rather than inferred from a
transcript-tail trigger. A durable start captures an immutable branch cut plus
the pre-minted compact prompt id, provider-qualified model, and standalone
operation. The successful boundary repeats this harness-stamped tuple; core
accepts new boundaries only when all six transaction/cut/suffix/prompt/model/
operation fields are present, the transaction resolves its start,
cut/prompt/model/operation match it, operation is standalone, `suffix_end`
equals the boundary parent, and cut is its ancestor. Legacy boundaries have
all six absent. Partial groups, unknown transactions, mismatches, and duplicate
outcomes are rejected identically during live validation and replay. Runtime
connection ids are deliberately not persisted:
they identify a daemon incarnation rather than durable provider work.
Only the start's post-commit reaction materializes one cut-local compact request
with that exact prompt id, provider-qualified model, operation, model parameters,
tool surface, accounting identity, and synthetic trigger. The harness then
completes bounded admission of the compact `agent.prompt_started` fact; only that fact's
one-shot live post-commit continuation may deliver the full request to the
selected provider. Mutable `:model` selection applies only to future work; it
cannot rewrite a committed start. If the captured model route disappears, the
transaction durably fails before any provider request is delivered. Success installs a
cut/suffix-bearing boundary so facts
committed during compaction survive after the replacement window. Terminal
failure records a safe durable category, suppresses automatic retry, and leaves
the agent addressable for ordinary prompts and explicit `:compact` or authorized
`agent_compact` recovery. A transaction remains runtime-blocked only while it
owes a durable continuation or background completion. Self/manual, cross-agent,
side-agent, and reactive-overflow failure handling removes that block only after
the owed completion is staged or delivered at its owning durable boundary.
Terminal failures with no remaining continuation obligation become usable
immediately; replay reconstructs the same result and never redispatches the
terminal provider request.
Inference resumes only after a durable dispatch watermark commits.
While that checkpoint is interceptable or waiting to persist, an explicit
`AwaitingCheckpoint` runtime state blocks every ordinary dispatch path.
New-format inference checkpoints carry provider-qualified model, inference
operation, and activation cut as one all-present ownership group alongside
their prompt ID, transaction owner, and transcript head. Legacy all-three-absent
ownership groups remain replay-compatible but cannot substitute current model
ownership; partial groups are invalid. A continuation for a successful
standalone transaction is accepted only when its model equals the start model,
its operation is inference, and its activation cut equals the start cut. Core
rejects incomplete or transaction-mismatched ownership correlations. The
checkpoint post-commit continuation uses that exact model for prompt
materialization regardless of later selection changes; provider delivery still
requires the matching admission-complete `agent.prompt_started` fact and its one-shot
live continuation. An unavailable route is durably terminalized before remote
send. It acknowledges
only materialized typed-message wakes on that branch, including sequence-keyed
canonical agent-message wakes governed by
[SPEC-agent-message-delivery](SPEC-agent-message-delivery.md), through the
watermark. Replay folds transaction outcomes and inference
responses in core; an uncompleted checkpoint restores as dispatch-uncertain
rather than being silently duplicated.
This materialization gate is governed by
[SPEC-compact-prompt-materialization-authority](SPEC-compact-prompt-materialization-authority.md).

If persistence rejects a completion-bearing steer after successful compaction,
the harness retains the exact interceptor-approved failed event, untouched
remaining steer suffix, and transaction ownership while the transaction remains
Running. Owning-branch retry recommits that approved event without rerunning its
completed interception chain, then sends only the untouched suffix through
ordinary publication and interception. If persistence rejects its continuation
checkpoint, the harness retains the exact `AwaitingCheckpoint` tuple. Neither
path retries off branch: owning-branch reselection republishes one exact suffix
or checkpoint, and ownership transfers only after the durable successor commits.
In-flight attempt markers are ephemeral and clear on every noncommit.
Agent unload or rollover may destructively cancel an already-delivered
interception request for a checkpoint or completion envelope. The registration
then remains installed but its connection is bypassed until exactly one stale
reply is consumed; registration replacement, indefinite suspension, and
disconnect reset follow
[SPEC-tau-harness-event-processing](../crates/tau-harness/specs/SPEC-tau-harness-event-processing.md).

Crash recovery after a successful compaction but before its continuation
checkpoint retains the transaction's exact model, cut, and owed transcript
watermark in `AwaitingCheckpoint`. Provider snapshots may arrive in stages.
The final pre-Ready snapshot determines presence; a model advertised by any
earlier staged snapshot and omitted by the final one counts as explicitly
removed. A model absent throughout staging remains unresolved until discovery
completes. Intermediate absence cannot suppress final captured-model presence.
Discovery completion, or an explicit removal from the model's prior provider
snapshot, is authoritative:
the harness commits one fully qualified checkpoint and either dispatches its
captured route or durably terminalizes it through the normal unavailable-route
path. Current role or `:model` selection never substitutes another model.

Canonical submitted, injected, and steered transcript facts carry a
harness-owned `inference_activation` bit. Typed pending-prompt provenance—not
prompt text or peer input—decides the bit: active work is true, while passive
background and restore context is false. Interceptors may rewrite sanctioned
text but cannot change the bit. Missing legacy fields deserialize false.
Replay considers only true facts after the last completed checkpoint; an
uncompleted checkpoint remains uncertain and is never automatically resent.

Committed compaction replacement windows contain the provider items that the
compacting request actually consumed. Replay does not reinterpret or rewrite
those materialized items when a later release changes source-based presentation.
Provider-authored opaque `Compaction` items are valid members of a nonempty,
structurally closed standalone replacement window and retain their raw replay
sidecars. Harness-authored `CompactionTrigger` items, malformed messages, and
open, duplicate, or otherwise incomplete tool rounds remain invalid. A standalone
terminal commits a replacement only on `EndTurn` with neither provider error nor
typed failure; error and typed failure take precedence as provider failures.
The canonical `agent.compacted` boundary may also carry optional harness-owned
before and after token measurements. Each count records whether it is exact
provider-reported usage or an estimate. Before-count precedence is canonical
provider prompt/input usage, then a prior context-usage baseline only when its
model and selected-branch ancestry still apply. After-count precedence is
canonical provider compacted-output usage, including a reported zero, then the
replacement-window byte estimate. The measurements are content-free display
metadata: they do not affect replacement authority or replay, and missing fields
on older boundaries remain valid. Because they live on the at-most-once boundary,
live publication, late catch-up, and cold replay expose the same values without
publishing the private standalone provider response.
Rejected terminals retain the transaction cut/resume and context/cache baselines,
while their report/accounting behavior is owned by
[SPEC-provider-execution-reports-and-canonical-facts](SPEC-provider-execution-reports-and-canonical-facts.md).
Typed suffix facts use the current projection, so a historical raw user prompt or
`<tau_message>` may coexist with newly projected `<user>` or external `<message>`
items across the compaction boundary. New compactions preserve their actual
provider-visible input. See
[SPEC-interactive-user-prompt-envelope](SPEC-interactive-user-prompt-envelope.md)
and
[SPEC-external-message-reports-and-facts](SPEC-external-message-reports-and-facts.md).

ChatGPT v2 is the scoped exception to provider-output-only replacement. It
sends the full provider-visible input plus a final `compaction_trigger`, accepts
exactly one provider compaction item followed by `response.completed`, and
constructs the replacement from approved retained input plus that item last.
Retention preserves order and metadata for real user/hook messages and
non-final agent messages no larger than 10,000 approximate tokens. It applies a
newest-first 64,000-token aggregate text budget, keeps complete groups, and
middle-truncates at most one boundary message with an explicit token marker.
Images and audio inside retained messages remain uncharged by this retention
budget. All other input items are omitted. Invalid output or failed validation
installs nothing.
Outside the explicitly scoped ChatGPT-v2 and Tau-owned cache-aligned summary
exceptions, a standalone provider request is stateless and its
ordered provider output remains the canonical replacement window without
pruning or reinterpretation. The default Tau-owned local compactor transforms one bounded assistant final text into exactly one synthetic user-role checkpoint message with no wrapper or supplement.

Cold agent rehydration restores context usage only from the latest
model-qualified durable assistant response on the selected branch and never
across a later compaction boundary. The producing model travels with the
runtime usage until provider model discovery can validate it against the
agent's resolved model. A qualified model that has not been discovered yet is
unresolved, not mismatched, so staggered provider startup retains it until its
provider appears; only a confirmed different resolution clears it. Live
navigation performs the same selected-branch derivation after the durable head
move, publishes the complete reconciled context-usage and agent-stats
projections, and restores a prior branch's qualified baseline when that branch
is reselected. A baseline is applicable only while its producing head is an
ancestor of the selected head; root-qualified usage applies to every branch.
Proactive scheduling and context-limit telemetry share that ancestry decision
and decline a baseline when its branch ownership cannot be established.
Accepted compaction and explicit agent model changes also clear the usage,
head, model, cached-token, and percentage baseline. Consequently live
navigation and cold rehydration make the same projected standalone-compaction
decision for the selected branch.

Typed image tool results remain canonical replay content until a compaction
replacement window omits them. Logical canonical image bytes are counted across
the agent's complete append-only history, including branches and replacement
windows; appends above the 128 MiB per-agent bound fail before persistence.
Agent-record writes must also satisfy the loader's 64 MiB encoded-record bound.
Provider request lowering independently enforces its raw-image and data-URL
aggregate limits.

Proactive context projection serializes byte-free provider transcript structure
under the existing one-byte-per-token conservative bound, then adds each image's
canonical encoded byte length and one token per rounded-up 32-by-32 patch.
Provider-visible tool-result rendering contributes to this structure, while the
parallel raw structured payload retained for non-provider consumers does not.
Exact serialized transcript-growth bytes remain separate telemetry and still
include that raw payload; they never substitute their JSON representation for
projected tokens. Threshold-fired standalone compaction persists
`automatic_threshold`; only explicit UI compaction retains the legacy/default
`manual` trigger.

Named automatic-compaction policies are harness-scheduled standalone policies.
The built-in named `default` policy runs at `before_inference` at the
adapter-published context-limit-safe threshold. Other named policies augment it;
only disabling `default` or legacy replace-all disabling removes that safety
policy.

Protected automatic scheduling measures the active provider-visible window from
scratch. Core reconstructs that logical window by folding each durable
replacement over its selected logical prefix; it never substitutes physical
journal ancestry for `replacement + preserved suffix`. When the window exceeds
the scheduling guard, Tau linearly selects its latest provider-closed logical
position within the adapter's nonzero `standalone_compaction_prefix_budget`.
Complete tool call/result rounds and each replacement are indivisible. The
durable start records the selected logical node as `cut` and the exact source
window head as `resume_through`, so success installs `compact(P)` followed by
the exact logical suffix. A later pass may therefore cut inside a suffix that
physically precedes its earlier replacement boundary without resurrecting the
old prefix. No fitting progress-making group produces a bounded typed failure
without provider work. The sole exception is an active window containing only
an earlier replacement: another automatic pass cannot consume transcript or
make progress, so an already-durable activation proceeds to ordinary inference
and leaves provider context-limit recovery authoritative. An absent budget
disables these size-recoverable automatic paths but not explicit/manual
compaction.

After each successful durable boundary Tau remeasures at the protected
continuation seam using the same effective numeric or provider threshold. If
the active window remains over the guard, that successful transaction owns one
durable `automatic_continuation` start before any inference checkpoint. Each
pass must consume another closed suffix group, which proves termination without
an arbitrary pass ceiling. A deterministic preflight failure and its reason are
recorded in the start itself, so live execution and restart commit the same
terminal outcome without provider work. A failed provider pass is never
recursively scheduled as another automatic compaction transaction. Its provider
request may use the bounded transient retry policy above; exhausted transient
failures, deterministic provider failures, serialization failures, and
indivisible-item failures remain explicit terminal outcomes.

`before_inference` policies otherwise retain the deferred runtime behavior above.
`outer_turn_finished` policies that match the logical terminal status coalesce
into one `automatic_compaction_decision` on the final canonical
`provider.response_finished`, or on the harness-authored canceled
`agent.prompt_terminated` when no provider response exists. The decision persists
one transaction identity, outer-turn identity, resolved model, and lowest matching
resolved threshold; its rule names and general work-status state remain
runtime-only. A response's assistant node supplies its exact cut; a canceled
prompt termination uses its durable parent because it appends no transcript node.
The matching `agent.outer_turn_finished`
references and makes the decision runnable, and exactly one
`agent.standalone_compaction_started` with `automatic_policy` claims it.

Replay repairs terminal decision -> outer-turn finish -> protected start without
reconstructing work status. A rejected append retains the same authority. A
selected sibling before start durably closes the decision as `stale_branch`
without provider work; descendant input remains suffix data. Eager authority,
an active transaction, and a blocked transaction suppress deferred fallback.
Reactive context-overflow authority and an inline compaction boundary suppress
eager decision creation at that terminal.

Named context-size alerts are advisory and independent of automatic compaction.
They use provider-reported input-token usage from successful completed ordinary
inference that did not itself install an inline compaction boundary. Failed and
compaction responses do not create alert work, and any compaction boundary resets
runtime crossing eligibility with the rest of context accounting. Their
configured internal prompt does not itself authorize or initiate compaction. The
default text asks the agent to use the separately authorized `compact` tool after
finishing its current task. When the prompt actually reaches the agent, its
existing durable submitted or steered fact carries the
`context_size_alert` internal kind. UIs render that fact in journal order as
a `□ <exact configured text>` notice in the dedicated internal-notice style
during live delivery and replay.
Crossing, queued-delivery, and one-shot suppression state remains runtime-only;
cleared alerts do not gain synthetic history.

Output-length continuation and context recovery have separate authority.
Compaction output never creates output-length eligibility, and a `Length`
terminal never masquerades as context overflow. If the reserved successor
receives the existing eligible no-output context-window rejection, reactive
compaction may run under its existing one-shot rules, but the outer turn's
output-length budget remains spent until a successful selected-branch ordinary
inference response commits an accepted foreground tool round. Compaction,
prompt counts, tool results, and off-branch responses do not rearm it; a later
`Length` before that action boundary is terminal incomplete.
When the reserved successor is context-rejected, its canonical response carries
only `recovery_disposition`; it does not also claim an output-length terminal.
The exact inference checkpoint owned by the successful reactive-compaction
transaction remains in the original output-length lineage and may publish its
terminal. Unrelated descendants cannot claim that lineage.
Compaction and cold context reconstruction replay retained full reasoning plus
the exact internal continuation steer; summary-only reasoning never becomes
continuation authority.
