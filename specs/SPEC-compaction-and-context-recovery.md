# SPEC-compaction-and-context-recovery: Compaction and context recovery

## Record justification

Compaction and recovery span core transcript cuts and boundaries, harness
transaction/checkpoint ownership, provider execution, typed message activation,
context accounting, cold restore, and CLI/model authority. No component-local
documentation can state the complete closed-prefix and recovery contract.

Typed image tool results are indivisible members of their existing closed
call/result round. Durable canonical bytes replay through normal inference and
standalone compaction using the same provider converter. Approximate context
accounting includes encoded bytes and conservative 32-by-32 image patches;
provider adapters separately enforce aggregate canonical-image and generated
data-URL request bounds. A compacted replacement may summarize an old image
away like any other input fact. This behavior is confirmed by
[DECISION-typed-image-tool-results](DECISION-typed-image-tool-results.md).

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

Harness-owned `compact` and `agent_compact` requests persist acceptance before
acknowledgement. Self requests start only from the complete tool-round
continuation gate, preserving tool-call/result adjacency. Cross-agent requests
may start for idle or explicitly blocked loaded targets and never queue behind
busy or dispatch-uncertain work. The target-scoped standalone transaction is
the provider-work authority; its terminal event produces exactly one
background completion for the original call before any self continuation
checkpoint.
The model-callable path accepts work only when the exact captured
provider-qualified model supports standalone compaction and its route exists.
It has no inline fallback. Provider terminal errors, including context-window
rejection during standalone compaction, produce one terminal transaction
failure and are not retried indefinitely.
An explicit `:compact` or authorized `agent_compact` request may recover a
terminally blocked standalone transaction. Its successor may preserve the
failed cut or retreat it along the same ancestor path to obtain a closed
provider prefix, but may never advance or cross branches and may not drop a
retained resume obligation. Its resume watermark must remain on the original
owed branch, and explicit recovery refuses a current head reached by navigating
away from that branch. Ordinary activating input remains queued and does
not clear or implicitly retry the block; `:cancel` does not abandon this idle
durable obligation.
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
completes the foreground append of the compact `agent.prompt_started` fact; only that fact's
one-shot live post-commit continuation may deliver the full request to the
selected provider. Mutable `:model` selection applies only to future work; it
cannot rewrite a committed start. If the captured model route disappears, the
transaction durably fails before any provider request is delivered. Success installs a
cut/suffix-bearing boundary so facts
committed during compaction survive after the replacement window. Terminal
failure records a safe durable category, blocks the owed activation from
automatic retry, and leaves the agent addressable for explicit `:compact` or
authorized `agent_compact` recovery.
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
requires the matching write-complete `agent.prompt_started` fact and its one-shot
live continuation. An unavailable route is durably terminalized before remote
send. It acknowledges
only materialized typed-message wakes on that branch, including sequence-keyed
canonical agent-message wakes governed by
[SPEC-agent-message-delivery](SPEC-agent-message-delivery.md), through the
watermark. Replay folds transaction outcomes and inference
responses in core; an uncompleted checkpoint restores as dispatch-uncertain
rather than being silently duplicated.
This materialization gate is governed by
[DECISION-compact-prompt-materialization-authority](DECISION-compact-prompt-materialization-authority.md).

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
[DECISION-interceptor-stale-reply-suspension](DECISION-interceptor-stale-reply-suspension.md).

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
Typed suffix facts use the current projection, so a historical raw user prompt or
`<tau_message>` may coexist with newly projected `<user>` or external `<message>`
items across the compaction boundary. New compactions preserve their actual
provider-visible input. See
[DECISION-interactive-user-prompt-envelope](DECISION-interactive-user-prompt-envelope.md)
and
[DECISION-common-external-message-envelope](DECISION-common-external-message-envelope.md).

Cold agent rehydration restores context usage only from the latest
model-qualified durable assistant response on the selected branch and never
across a later compaction boundary. The producing model travels with the
runtime usage until provider model discovery can validate it against the
agent's resolved model. A qualified model that has not been discovered yet is
unresolved, not mismatched, so staggered provider startup retains it until its
provider appears; only a confirmed different resolution clears it. Accepted
compaction and explicit agent model changes also clear the usage, head, model,
cached-token, and percentage baseline. Consequently the first post-resume
activation runs the same projected standalone-compaction decision as a live
agent.

Typed image tool results remain canonical replay content until a compaction
replacement window omits them. Logical canonical image bytes are counted across
the agent's complete append-only history, including branches and replacement
windows; appends above the 128 MiB per-agent bound fail before persistence.
Agent-record writes must also satisfy the loader's 64 MiB encoded-record bound.
Provider request lowering independently enforces its raw-image and data-URL
aggregate limits.

Proactive context projection serializes byte-free transcript structure under
the existing one-byte-per-token conservative bound, then adds each image's
canonical encoded byte length and one token per rounded-up 32-by-32 patch.
Exact serialized transcript-growth bytes remain separate telemetry and never
substitute their JSON byte-array expansion for projected tokens. Threshold-fired
standalone compaction persists `automatic_threshold`; only explicit UI
compaction retains the legacy/default `manual` trigger.

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
`[tau-internal]: <exact configured text>` during live delivery and replay.
Crossing, queued-delivery, and one-shot suppression state remains runtime-only;
cleared alerts do not gain synthetic history. This behavior is confirmed by
[DECISION-context-size-alert-history](DECISION-context-size-alert-history.md).
