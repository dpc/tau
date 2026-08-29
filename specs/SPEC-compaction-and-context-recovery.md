# SPEC-compaction-and-context-recovery: Compaction and context recovery

## Record justification

Compaction and recovery span core transcript cuts and boundaries, harness transaction/checkpoint ownership, provider execution, typed message activation, context accounting, cold restore, and CLI/model authority, so no component-local documentation can state the complete closed-prefix and recovery contract.

The user explicitly approved the durable failure, retry, suppression, replay,
and successor semantics added for ticket `9dvw`, satisfying
[GATE-persistence-and-extension-interface-change-approval](GATE-persistence-and-extension-interface-change-approval.md).
The user separately approved the bounded, durable rolling recovery semantics
for ticket `b1yw`, including provider-rejection authority, per-pass progress,
replay, and typed no-progress termination, satisfying the same gate.
The user also approved the native Codex semantic-progress retry and cost
boundary for ticket `gtdq`, satisfying the same gate.
The user separately approved the Chat Completions local-summary semantic-idle
and absolute request deadlines for ticket `boo4`, satisfying the same gate.
The user separately approved public Responses max-output incompletion,
continuation, truncated-tool, accounting, retry, and standalone-compaction
semantics for ticket `8ds8`, satisfying the same gate.

Typed image tool results are indivisible members of their existing closed
call/result round. Durable canonical bytes replay through normal inference and
standalone compaction using the same provider converter. Provider adapters
enforce their native exact byte resource bounds independently; Tau does not
translate image bytes or patches into token usage. A compacted replacement may
summarize an old image away like any other input fact. This behavior is confirmed by
[GATE-typed-image-tool-results](GATE-typed-image-tool-results.md).

The Tau-owned summary fallback for Chat Completions, OpenRouter, and public
Responses models uses cache-aligned ordinary-prefix compaction. Its generic
profile publishes no prefix byte cap or proactive threshold; its output-token
cap is derived only within the token domain and its narrative byte cap is an
independent resource bound. An explicit `local_summary_compaction` profile may
publish native-domain overrides. Provider-native ChatGPT/Codex compaction
remains preferred and unchanged.

The compaction request lowers the selected immutable cut exactly as ordinary
inference does: the same system prompt, tool definitions, ordered typed history,
images, raw tool-call arguments, route/model fields, and configured cache
controls. Before lowering, the provider adapter requires exactly one
`CompactionTrigger` in the complete standalone context, as the sole item of the
final `UserInput` block. Missing, malformed, non-final, duplicated, and mixed
triggers reject the request. The adapter replaces that trigger with one
harness-authored user message last, inside
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
Before validation, local-summary sampling exposes only the existing bounded
content-free byte/timing statistics and status/activity signals at their existing cadence;
assistant text, reasoning, tool, and opaque output remain private. Invalid and
canceled attempts expose no content-bearing update. Ordinary inference streaming
and opted-in private provider debug capture remain unchanged.
The immutable cut, transaction, suffix preservation, exact-once commit, and
live/cold replay contracts remain unchanged: a committed summary is replayed
after restart without another model call. Ordinary opted-in provider debug
capture policy also applies to local compaction and remains non-semantic
observability data.
Local-summary Chat Completions requests share the ordinary Chat Completions
five-minute semantic-idle deadline and non-renewable thirty-minute absolute
deadline from backend dispatch. Newly accepted nonempty narrative or reasoning
renews idle time; content-free stream activity does not. Local-summary privacy,
cancellation, and timeout/retry classification remain unchanged.

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
originating prompt and tool call. Each carries a bounded request ID unique
within its target agent journal and immutable caller, target, model, tool-call,
accepted-head, and prompt correlation. The durable correlation identity is
therefore `(target_agent_id, request_id)`; harness-global projections must not
use the target-local request ID alone. Provider and extension input cannot
select a cut, model, or caller, and exactly one durable start or pre-start
failure may claim an accepted request. Projection exposes waiting, started
(including transaction outcome), and failed state so every crash window can be
repaired without resending ambiguous provider work or duplicating a background
completion.
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

An ordinary inference that receives a canonical, no-output context-window rejection may authorize one durable standalone-compaction chain when the captured model still matches, advertises standalone support, and role policy permits compaction. The terminal response and recovery disposition commit before a uniquely correlated compaction start. A context-rejected automatic standalone request durably pre-mints one successor at the immediate previous useful provider-closed cut. Rejection exhaustively repeats that strict retreat until one request succeeds or the history is irreducible. A successful pass then advances toward the immutable logical target by consuming the replacement plus at least one more closed suffix group. The rejected activating input remains in the suffix under the original resume watermark. The finite preceding transcript bounds both phases: retreat strictly moves backward, and forward rolling strictly removes surviving groups. Typed preflight or irreducible failures terminate without recursive inference retry. Inference resumes only after the chain reaches the end of the logical provider window preceding the rejected activation.

Compaction dispatch and continuation reuse the existing durable transaction machinery. A committed partial chain remains owed if its captured route or standalone capability disappears; replay commits one predecessor-linked typed `route_failed` terminal without provider dispatch instead of checkpointing inference. Standalone-compaction overflow, a post-chain inference overflow, a second overflow, and ambiguous dispatch are terminal rather than recursive. Partial output, cancellation, unsupported policy, legacy checkpoints, and branch/model mismatch never authorize recovery. Replay resumes an unclaimed planned recovery once, continues a committed successful partial chain from its durable predecessor, treats an interrupted compact dispatch as blocked, and retains the existing dispatch-uncertain rule after inference dispatch.

## Manual compaction

UI `:compact` durably accepts one target-scoped manual intent before replying
`compaction queued`. A second request coalesces without another durable fact.
The request pins the selected agent, branch ancestry, ordinary generation, and
provider-qualified model. While an ordinary prompt or tool round is active, the
harness claims the request at the first provider-closed boundary: a complete
normal or cancelled turn, or a complete call/result round before post-tool
continuation inference. Already queued ordinary activation remains behind the
compaction. Model or role drift and sibling/rewound branch selection close the
request with a categorical pre-start failure; requester disconnect does not.
Cold replay restores an unclaimed request and permits exactly one durable start
or pre-start failure.

An already committed automatic recovery chain retains priority. New proactive
work is suppressed behind a queued UI intent. Automatic success satisfies the
manual intent without redundant provider work; an automatic failure or block
permits the pending explicit request to make one existing manual recovery
attempt. Provider and context-window failure consume that attempt, and explicit
manual context rejection never enters automatic strict-predecessor retreat.

UI `:compact` may preempt a busy target only when its sole remaining foreground
call is the same still-installed harness-owned exact, bare, or activating-input
`wait`. The harness exclusively claims that waiter, commits one canonical
cancelled terminal with null provider output, and starts ordinary manual
compaction only after that terminal closes the complete tool round. The
compaction provider may therefore read the closed cancelled round; later
inference sees the replacement window rather than an unresolved structural
wait.

Event-loop order chooses races. A wait result, activating input, timeout, or
ordinary cancellation that settles first leaves the durable intent queued. A
compaction claim that wins excludes those wait terminals, coalesces repeated
requests, and runs before queued activation dispatch. Cancellation append
failure restores ordinary wait arbitration while the durable request remains
owed. Teardown clears only the transient waiter claim. Once
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
failure and are not retried indefinitely. Before any semantic compact output is
accepted, standalone compaction uses the shared transient-failure classifier
and jittered Fibonacci scheduler with a named five-attempt policy, including the
first attempt. A same-event error processed before content is accepted remains
pre-progress. After semantic compact output is accepted, a later failure
discards the uncommitted output and terminalizes without automatic retry;
recovery requires a distinct explicit request. Deterministic failures,
including context-window exhaustion, are terminal immediately. Ordinary
inference deliberately retains its unbounded transient-retry policy.
The Codex adapter serializes the first v2 compaction probe for a route/account
generation. A compaction-specific request rejection removes standalone
capability for that generation; explicit tools and automatic recovery share the
downgrade. The provider publishes that generation-negative state separately
from routes that never support standalone compaction. Automatic compaction
treats it as unavailable, while an explicit `:compact`, `compact`, or authorized
`agent_compact` request may ask the provider to refresh the credential/account
identity. An unchanged identity retains the negative observation without a
network probe. A changed identity invalidates only the stale observation and
admits one serialized fresh probe for the new generation; concurrent explicit
requests coalesce behind its capability result, then each successful waiter may
compact its own context. A compaction-specific rejection marks the new
generation negative and fails every waiter without redundant probes. Identity
refresh is request-driven: no proactive polling, unrelated inference, or model
republication is required. Negative capability evidence is not persisted.
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
provider-qualified model and operation. Optional provider-reported token usage
and exact serialized transcript-delta bytes remain separate native-domain facts.
Missing, zero, stale/model-changed, or contradictory token inputs produce
`insufficient_evidence`; bytes never stand in for tokens. The record makes
advertised-limit drift observable but does not feed back into calibration.
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
optional provider token counts/window, optional exact serialized
transcript-growth bytes, active threshold, closed policy/eligibility/action, and
a closed observation enum. Each field is absent only when its own exact
authority is unavailable. A categorical observation requires a positive
advertised limit and nonzero provider input usage. Raw prompts, errors, response
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
JSON. Every completed or durable opaque provider item carries required raw JSON
that parses to the same semantic value as its structured value and whose
provider `type` matches the outer reasoning, compaction, or unknown-item family.
Missing, malformed, kind-mismatched, and semantically contradictory input fails
validation; Tau neither synthesizes a legacy representation nor uses one
representation as fallback for the other. Harness-authored `CompactionTrigger`
items, malformed messages, and
open, duplicate, or otherwise incomplete tool rounds remain invalid. A standalone
terminal commits a replacement only on `EndTurn` with neither provider error nor
typed failure; error and typed failure take precedence as provider failures.
The canonical `agent.compacted` boundary may carry provider-reported compact
request input and output token counts. These content-free display/accounting
fields never become scheduling authority; absent provider usage remains absent.
Legacy `compacted_input_tokens` decodes as the provider output count, while new
records encode `compaction_output_tokens`. Because the fields live on the
at-most-once boundary, live publication, late catch-up, and cold replay expose
the same values without publishing the private standalone response.
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
non-final agent messages no larger than 40,000 UTF-8 bytes. It applies a
newest-first contiguous 256,000-byte aggregate text budget, keeps complete
groups, and middle-truncates at most one boundary message with a byte-labeled
marker.
Images and audio inside retained messages remain uncharged by this retention
budget. All other input items are omitted. Invalid output or failed validation
installs nothing.
Outside the explicitly scoped ChatGPT-v2 and Tau-owned cache-aligned summary
exceptions, a standalone provider request is stateless and its
ordered provider output remains the canonical replacement window without
pruning or reinterpretation. The default Tau-owned local compactor transforms one bounded assistant final text into exactly one synthetic user-role checkpoint message with no wrapper or supplement.

Cold agent rehydration restores the latest ordinary durable assistant
observation on the selected branch and never crosses a later compaction
boundary. Missing or zero usage on that newest response blocks fallback to an
older count. The producing prompt id and model travel with nonzero runtime usage
until provider model discovery can validate them against the agent's resolved
model. Live navigation performs the same selected-branch derivation and
publishes the complete reconciled context facts. Proactive scheduling and
context-limit telemetry share that ancestry/newest decision and decline evidence
when branch ownership cannot be established. Accepted compaction and explicit
model changes clear the observation. Live navigation and cold rehydration
therefore make the same exact-evidence decision.

Typed image tool results remain canonical replay content until a compaction
replacement window omits them. Logical canonical image bytes are counted across
the agent's complete append-only history, including branches and replacement
windows; appends above the 128 MiB per-agent bound fail before persistence.
Agent-record writes must also satisfy the loader's 64 MiB encoded-record bound.
Provider request lowering independently enforces its raw-image and data-URL
aggregate limits.

Proactive compaction requires a nonzero provider-reported ordinary-input token
count at or above the selected exact `TokenCount` threshold. New durable starts
carry the correlated provider prompt, reported count, threshold, and threshold
source. Transcript bytes, image dimensions, and local summaries never authorize
threshold scheduling. Exact serialized transcript-growth bytes remain
independent telemetry. Threshold-fired standalone compaction persists exact
evidence; only explicit UI compaction retains the legacy/default `manual`
trigger.

Named automatic-compaction policies are harness-scheduled standalone policies.
The built-in named `default` policy runs at `before_inference` at the
adapter-published context-limit-safe threshold. Other named policies augment it;
only disabling `default` or legacy replace-all disabling removes that safety
policy.

Protected proactive scheduling uses only a nonzero provider-reported input count
from an accepted ordinary request for the same selected model. The durable start
records the exact provider prompt, count, threshold, and threshold source. Core
reconstructs and validates that evidence on replay. Transcript growth,
replacement output, serialized bytes, image dimensions, and local summaries
never supply or extend token authority. Missing or zero provider usage therefore
runs ordinary inference. A proactive success also runs inference; it never
remeasures locally or independently schedules a second pass.

An adapter's optional `standalone_compaction_prefix_budget` is an exact
`ByteCount` work/resource limit, not a token guard. When present, Tau selects the
latest provider-closed logical prefix whose fully materialized historical
context fits it. Complete tool call/result rounds and each replacement remain
indivisible. When absent, Tau dispatches the full selected closed prefix and lets
the provider's canonical token-capacity rejection decide recovery. The durable
start records `cut` and `resume_through`, so success installs `compact(P)`
followed by the exact logical suffix. No fitting progress-making group produces
a bounded typed preflight failure without provider work.

Only a canonical typed, no-output `context_window_exceeded` terminal authorizes
automatic retreat. Its failure record pre-mints one successor at the immediate
previous useful provider-closed cut. Each rejection strictly retreats; the first
success begins target-based forward rolling, where each successful continuation
consumes another closed suffix group. These monotonic phases prove termination
without a pass ceiling. Generic provider failures, cancellation, route loss,
manual compaction, and irreducible history terminalize without automatic retry.
A deterministic byte-budget preflight failure records its reason in the start,
so live execution and restart commit the same terminal without provider work.

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

Canonical public Responses `max_output_tokens` incompletion is the existing
`Length` terminal. Ordinary inference preserves validated partial prose,
reasoning, usage, and response identity, never executes a truncated tool call,
and never retries the unchanged request. A reasoning-only response whose
provider-native reasoning is replay-safe may claim the existing single bounded
output-length continuation; prose or tool output cannot. Standalone compaction
preserves the incomplete terminal for accounting but never splices its partial
window or retries it automatically. Unknown incomplete reasons remain provider
failures.
