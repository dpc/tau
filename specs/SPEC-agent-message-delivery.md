# SPEC-agent-message-delivery: Agent-message delivery and transcript projection

## Record justification

Agent-message behavior spans protocol events and peer RPC, harness admission and post-commit activation, core journal folding and branch placement, provider context assembly, watch lifecycle, checkpoint/compaction ownership, and cold replay, so no one component can describe the end-to-end authority, ordering, and liveness contract coherently.

## Authority and occurrence identity

`AgentMessageSent` is the sender-owned durable projection and
`AgentMessageReceived` is the recipient-owned durable projection. Their typed
sender, session, recipient, kind, structured watch state, and unescaped body are
the content authority. They remain separate from extension-owned `message.*`
facts.

Each event accepted into an owning agent journal creates exactly one canonical
semantic projection for its direction. Its identity is:

- `message_id` for logical sender/recipient, UI, and peer-RPC correlation;
- the owning journal plus `durable_event_seq` for one accepted occurrence;
- direction for inbound/outbound distinction, including self-send;
- `NodeId` for the occurrence's materialized branch position; and
- peer `request_id` and capability only for live callback authentication.

The harness-owned `message` tool accepts only local agent, local session,
remote session, and remote agent recipient forms. The exact raw recipient
`user` is unsupported and fails before routing or creating either durable
projection.

No delivery-created prompt, tool output, wake, or control event repeats the
body. A self-send creates outbound then inbound occurrences with distinct
journal sequences. An accepted retry may create another occurrence with the
same logical ID. Provider-authored tool arguments and compaction summaries are
not agent-message projections.

## Semantic placement

Live append and replay give the fold the persisted event sequence. After
existing ownership and kind/state validation:

1. An agent tree admits at most one open foreground provider tool round, while
   still allowing one response to request multiple parallel tool calls.
2. While that round is open, provider inference is blocked on every branch. A
   second tool-bearing provider response is rejected before persistence and
   launches no tools.
3. A journal record marked `InferenceDeferredInputV1` opens one exact ordinary
   inference owner. Covered context occurrences are
   `AgentPromptSubmitted`, `AgentUserMessageInjected`, `AgentPromptSteered`,
   `AgentMessageSent`, `AgentMessageReceived`, and successfully projected
   canonical `message.*` facts. This includes nonactivating context notices.
   Control events, compaction events, provider output, tool terminals, and
   standalone-provider work are excluded. While the owner is unresolved, a
   covered occurrence accepted on its branch remains node-less; receipt and
   publication still complete immediately.
4. A non-tool response materializes at the owner's checkpoint head and then
   drains those inputs in durable sequence order. A tool-bearing response
   transfers them to the tool barrier.
5. Without a marked owner or open round, sent and received projections
   materialize at the accepted parent. Historical records use this legacy rule.
6. With an open round, a projection remains pending only when the tool-calling
   assistant is equal to or an ancestor of its accepted parent. Root inputs,
   inputs above the assistant, and sibling-branch inputs materialize immediately
   and are never drained by that round.
7. Once every call has a terminal result, one complete `ToolResults` aggregate
   materializes directly after the tool-calling assistant.
8. Applicable pending agent messages and extension message facts then materialize
   in their common durable acceptance order, while retaining distinct typed
   variants.
9. `AgentPromptTerminated(Canceled | Stale)` closes a marked owner without an
   assistant block and materializes pending inputs from their accepted branch
   positions. This private durable closure folds during agent cold replay but is
   excluded from historical subscriber catch-up.

The same rules cover errors, cancellation, and background-result closure. A
tool-calling assistant and its complete result aggregate are indivisible.
Sender projections created by a successful message tool therefore remain on the
active branch after its compact acceptance result. That result includes the stable
message ID and explicitly says that recipient response is not guaranteed.

Only newly marked ordinary checkpoints use inference-owned placement. Legacy
checkpoints preserve commit-order placement and node IDs. The private journal
marker never enters provider or configured-extension DTOs.

## Canonical provider rendering

Provider assembly renders typed materialized projections, not delivery-created
prompt text:

- outbound ordinary `Message` projections are omitted from the sender's provider
  context;
- local inbound `Message` uses user role and exactly this shape, with stable
  sender ID and an exact-close-framed body:

  ```text
  <tau_internal>You have received a message from <stable-agent-id>

  <message>
  <body with exact </message> collisions replaced>
  </message></tau_internal>
  ```

- cross-session inbound `Message` frames the authenticated stable sender
  session/agent identity and body in `<tau_peer_message>`, escapes that inner
  exact close, then frames and escapes the complete projection in outer
  `<tau_internal>`;
- `WatchResponse` and `WatchPrompt` retain separate sender-labelled typed
  wrappers and replace only their own exact closing sentinel in each body;
- current provider and work-status kinds render wording reconstructed from their
  structured state; current long-wait kinds render their harness-derived
  threshold; and
- `WatchLifecycle` renders its structured stopped state and reason without a body.

`watch_lifecycle` is present if and only if `kind` is `WatchLifecycle`, and its
ordinary message body is exactly empty.

Display names remain UI-only. Peer bodies remain agent-authored model input, not
harness instructions. Model-authored work titles receive trusted-frame visible
escaping before interpolation. Initial and redundant structured watch snapshots render
zero provider blocks.

All body text other than the current envelope's own exact close remains literal,
including ampersands, quotes, entity-like strings, nested tags, and other
families' close tokens. The outer `<tau_internal>` frame additionally replaces
every exact `</tau_internal>` collision after the inner body frame. Dynamic peer
attributes retain separate attribute-safe escaping. See
[SPEC-exact-sentinel-prompt-envelopes](SPEC-exact-sentinel-prompt-envelopes.md).

The omission affects provider context only. The sender-owned
`AgentMessageSent` fact remains durable, ordered, replayed, and delivered to
typed UI consumers. It does not alter the original `message` tool call or its
compact acceptance result. An agent recipient retains its authenticated
inbound user-role projection. UI rendering continues to consume the directional
facts under its existing message-display policy. Success reports harness
acceptance with the stable correlation ID; the sender and recipient projections
remain separate under the nontransactional crash boundary below. Success never
promises recipient inference, reply, or completion.

## Live activation and waits

After a live activating receive occurrence commits, the harness uses the exact
append outcome rather than searching by logical ID. It synchronizes an
immediately materialized recipient cursor and queues at most one runtime wake
for the durable sequence. A deferred wake is visible to pending-activation and
wait predicates but cannot dispatch until terminal closure resolves its
sequence to the typed node.

The wake is payload-free. It may contain only closed runtime ownership data such
as durable sequence, activation class, eventual node, and peer admission byte
weight. It contains no body, display name, sender-authored instruction, route
capability, or second authority claim.

The harness queues input before applying runtime trigger readiness. Each approved
source snapshots harness-owned idle, wait-any, and wait-tool monotonic deadlines
from one post-admission cut; later input and state changes never reset them.
Admission, folding, publication, peer acknowledgement, and activation observation
remain immediate. Only a trigger-ready selected-branch wake completes a registered
wait, preempts an otherwise eligible queued tool, enters no-provider failure, or
makes the agent runnable. Readiness is sticky.

Activation classes are:

- ordinary agent input for `Message`, `WatchResponse`, and `WatchPrompt`;
- isolated provider/work-status watch input for noninitial model-visible
  provider and work-status projections and typed lifecycle notifications; and
- no activation for initial or redundant structured watch snapshots.

Ordinary `Message`, `WatchResponse`, and `WatchPrompt` use the agent-message
policy; noninitial provider/work/long-wait/lifecycle notifications use status;
canonical external message facts use external-message. Unclassified and control
sources retain immediate behavior.

Explicit message intake never becomes watch-prompt fanout. Isolated current
provider/work-status watch turns retain cascade suppression.

## Checkpoints, branches, and compaction

Dispatch waits for every selected wake to materialize, then commits an
`AgentInferenceDispatchStarted.through` checkpoint. One trigger-ready
selected-branch occurrence makes the agent runnable, and the materialized prompt
opportunistically coalesces every already-admitted selected-branch wake at its
cut, including wakes whose own deadline is later. It acknowledges only message-fact and agent-message wakes
whose nodes are ancestors of its selected-branch watermark.

If provider and extension initialization instead settles with no available
models, the harness retires the same materialized selected-branch wakes plus any
pending replay activation without committing an inference checkpoint or creating
provider work. One Alert-purpose actionable provider-configuration failure covers
the coalesced activation for that agent. Off-branch wakes remain dormant and
owned; a later model publication permits new message activations and any
subsequently selected dormant wake to use ordinary dispatch.

Explicit navigation is allowed while activation is owed. If the selected branch
does not contain a wake node, that wake remains dormant, retains any live
admission ownership, and is not scanned into context or acknowledged. Reselecting
its branch makes it eligible again. Endpoint/session lifecycle cleanup, or the
settled-empty provider failure above, may retire the runtime wake.

The same branch ownership applies to committed activations parked behind
interception or context-readiness gates. Each obligation retains its captured
pre-activation cut and activation watermark. Only an obligation whose watermark
is an ancestor of the selected head may claim a checkpoint or compaction start.
Comparable selected-branch cuts choose the ancestor; sibling obligations remain
distinct and dormant rather than collapsing to a root cut or a CID-only token.
A successor transfers those obligations only after its durable commit. A delayed
successor that no longer belongs to the selected branch is rejected before
persistence and releases its runtime reservation without consuming the
obligation.

Compaction chooses a closed provider prefix and never separates a tool-calling
assistant from its result aggregate. A deferred message materializes after the
aggregate and cannot be consumed by a cut at the assistant. Activation-driven
compaction cuts before the earliest selected wake and carries `resume_through`
through the activation head. Continuation acknowledges only materialized wakes
covered on that branch. A failed successor cannot replace an owed watermark
with an ancestor or sibling. Facts accepted during standalone compaction remain
exact suffix content.

## Replay, recovery, and cleanup

Replay folds each canonical fact with the same sequence-aware placement and
rendering. It rebuilds one immediately trigger-ready payload-free wake for each uncovered activating typed
receive or canonical `message.*` occurrence; it does not recreate admission
ownership, wait completion, watch fanout/edge, private reply authority, or peer
retry. A marked uncertain ordinary dispatch with deferred activating input is
superseded by durable `AgentPromptTerminated(Stale)` before that wake can run;
Tau never resends the uncertain prompt. Without deferred activating input, the
existing uncertain block remains.

Completed-worker recovery derives outstanding/answered message work from typed
received occurrence nodes, selected-branch checkpoint coverage, and a later
terminal non-tool response. It does not match generated wrapper text. A later
uncovered receive remains outstanding.

Checkpoint coverage and settled-empty selected-branch retirement release the
exact wake and peer byte weight. Endpoint termination/unload, pending-start
cancellation, session rollover, and shutdown clear applicable runtime wakes,
waits, watch isolation state, and admission weights; committed facts remain.
Stale-generation completions cannot install wakes in a replacement session. If
this cleanup destructively cancels an already delivered interception request, the
responder follows the one-reply suspension contract in
[SPEC-tau-harness-event-processing](../crates/tau-harness/specs/SPEC-tau-harness-event-processing.md):
new publications bypass it until one stale reply is consumed, or disconnect
resets the connection.

No legacy journal is rewritten or heuristically deduplicated. Decodable old
`AgentPromptSubmitted`/`AgentPromptSteered` wrapper facts replay as recorded.
There is no migration, dual-read/write compatibility path, or body/message-ID
repair.

Cold replay applies the tree-global round and branch-applicability invariants
before any provider inference. A journal containing simultaneous open foreground
rounds fails closed; Tau does not infer ownership from branch selection, add a
compatibility schema, or repair the history heuristically. Cold recovery
terminalizes the sole open round even when its assistant is dormant on a sibling
branch, after which the existing node-producing result event may advance the
durable head.

## Routing, ACKs, and failure boundaries

Same-session sender and recipient publications remain separate and
nontransactional. Same-session `message` success means harness acceptance and
enqueue, not recipient provider work; a crash may leave sender projection/tool
result without the recipient projection. No automatic model-level ACK is
introduced. Cross-harness callback-bound typed authority and bounded admission
remain unchanged.

The target ACK becomes eligible after the exact receive occurrence completes
bounded persistence admission and its authoritative in-memory fold. It does not
wait for worker filesystem I/O, so an ACK or provider effect can survive a crash
that loses the journal fact. This boundary is governed by
[SPEC-semantic-journal-writeback-durability](SPEC-semantic-journal-writeback-durability.md).
The target's same-loop post-commit reaction queues or transfers the live wake
before sending that ACK, but model inference and response are not ACK
prerequisites. Only a confirmed target ACK permits the sender projection.
Target commit followed by ACK loss remains ambiguous: a retry may create a
second occurrence, wake, agent, model turn, and spend. Sender append failure
never rolls back the target. Tau adds no restart deduplication, distributed WAL,
or cross-journal transaction.

Peer count/body admission covers parked precommit receives and loaded or
pending-start uncheckpointed peer wakes. Commit transfers byte weight exactly
once from the pending continuation to the wake. Checkpoint or lifecycle cleanup
releases it exactly once. Restart drops runtime admission state with the wake.
