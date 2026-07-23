# SPEC-agent-message-delivery: Agent-message delivery and transcript projection

## Record justification

Agent-message behavior spans protocol events and peer RPC, harness admission and
post-commit activation, core journal folding and branch placement, provider
context assembly, watch lifecycle, checkpoint/compaction ownership, and cold
replay. No one component can describe the end-to-end authority, ordering, and
liveness contract coherently.

This specification implements
[DECISION-agent-message-transcript-projection](DECISION-agent-message-transcript-projection.md).

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
3. With no open round, sent and received projections materialize at the accepted
   parent.
4. With an open round, a projection remains pending only when the tool-calling
   assistant is equal to or an ancestor of its accepted parent. Root inputs,
   inputs above the assistant, and sibling-branch inputs materialize immediately
   and are never drained by that round.
5. Once every call has a terminal result, one complete `ToolResults` aggregate
   materializes directly after the tool-calling assistant.
6. Applicable pending agent messages and extension message facts then materialize
   in their common durable acceptance order, while retaining distinct typed
   variants.
7. The fold outcome ends at the last drained node so the existing live
   durable-head advance includes the result and every deferred input.

The same rules cover errors, cancellation, and background-result closure. A
tool-calling assistant and its complete result aggregate are indivisible.
Sender projections created by a successful message tool therefore remain on the
active branch after that tool's compact `Message sent` result.

An ordinary provider inference already in flight creates no broader deferral
transaction. If no tool round is open, a newly accepted projection materializes
in commit order; a response to an earlier provider request may append after it
even though that request did not observe it. The receive wake schedules later
provider work rather than reordering either durable occurrence.

## Canonical provider rendering

Provider assembly renders typed materialized projections, not delivery-created
prompt text:

- outbound `Message` uses assistant role;
- local inbound `Message` uses user role and exactly this shape, with stable
  sender ID and an exact-close-framed body:

  ```text
  [tau-internal]: You have received a message from <stable-agent-id>

  <message>
  <body with exact </message> collisions replaced>
  </message>
  ```

- cross-session inbound `Message` retains the authenticated
  `<tau_peer_message>` envelope and stable typed sender session/agent identity;
- `WatchResponse` and `WatchPrompt` retain separate sender-labelled typed
  wrappers and replace only their own exact closing sentinel in each body;
- `WatchTurnState` and `WatchProviderStatus` render only wording reconstructed
  from their structured state.

Display names remain UI-only. Peer bodies remain agent-authored model input, not
harness instructions. Initial and redundant structured watch snapshots render
zero provider blocks.

All body text other than the current envelope's own exact close remains literal,
including ampersands, quotes, entity-like strings, nested tags, and other
families' close tokens. Dynamic peer attributes retain separate attribute-safe
escaping. See
[DECISION-exact-sentinel-prompt-envelopes](DECISION-exact-sentinel-prompt-envelopes.md).

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

The harness queues input before completing the exact agent's registered wait.
The content-free wait result does not consume the wake. Existing eligible
queued-tool preemption may close a round first; foreground and partially queued
rounds are not preempted. Recipient arrival does not itself reset loop-failure
history. Side-agent completion drains owed activating wakes before releasing
the original start owner or tearing the endpoint down.

Activation classes are:

- ordinary agent input for `Message`, `WatchResponse`, and `WatchPrompt`;
- isolated lifecycle/provider-watch input for noninitial model-visible
  `WatchTurnState` and `WatchProviderStatus`; and
- no activation for initial or redundant structured watch snapshots.

Explicit message intake never becomes watch-prompt fanout. Isolated lifecycle
turns retain watch-cascade suppression.

## Checkpoints, branches, and compaction

Dispatch waits for every selected wake to materialize, then commits an
`AgentInferenceDispatchStarted.through` checkpoint. One checkpoint may coalesce
multiple ready wakes. It acknowledges only message-fact and agent-message wakes
whose nodes are ancestors of its selected-branch watermark.

Explicit navigation is allowed while activation is owed. If the selected branch
does not contain a wake node, that wake remains dormant, retains any live
admission ownership, and is not scanned into context or acknowledged. Reselecting
its branch makes it eligible again. Endpoint/session lifecycle cleanup may
retire the runtime wake.

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
rendering, but creates no wake, provider dispatch, wait completion, watch
fanout/edge, private reply authority, peer retry, or admission ownership. A
crash after receive commit but before its inference checkpoint intentionally
leaves context without automatic activation. A committed checkpoint uses the
existing dispatch-uncertain and provider-response recovery contract instead of
recreating a message wake.

Completed-worker recovery derives outstanding/answered message work from typed
received occurrence nodes, selected-branch checkpoint coverage, and a later
terminal non-tool response. It does not match generated wrapper text. A later
uncovered receive remains outstanding.

Checkpoint coverage releases the exact wake and peer byte weight. Endpoint
termination/unload, pending-start cancellation, session rollover, and shutdown
clear applicable runtime wakes, waits, watch isolation state, and admission
weights; committed facts remain. Stale-generation completions cannot install
wakes in a replacement session. If this cleanup destructively cancels an already
delivered interception request, the responder follows the one-reply suspension
contract in
[DECISION-interceptor-stale-reply-suspension](DECISION-interceptor-stale-reply-suspension.md):
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
result without the recipient projection. User delivery has only the sender
projection, and success is not proof that a UI observed it. No automatic
model-level ACK is introduced. Cross-harness callback-bound typed authority and
bounded admission remain unchanged.

The target ACK becomes eligible after the exact receive occurrence commits.
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
