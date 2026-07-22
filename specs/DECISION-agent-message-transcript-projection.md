# DECISION-agent-message-transcript-projection: Canonical agent-message transcript projections

Authority: confirmed, 2026-07-22, dpc

Each accepted `agent.message_sent` or `agent.message_received` journal
occurrence is the sole payload-bearing canonical semantic projection for that
direction in its owning transcript. Delivery does not create a second
`agent.prompt_submitted`, `agent.prompt_steered`, injected prompt, or activation
event containing the message body. A self-send intentionally produces one
outbound and one inbound projection in the same journal; cross-harness retries
may produce additional accepted occurrences under the existing at-least-once
contract.

The owning journal's durable event sequence identifies one projection
occurrence. Both directions use a distinct typed pending agent-message context:
Tau admits at most one open foreground provider tool round per agent tree and
blocks provider inference on every branch until it terminalizes. A committed
context input waits only when the round's tool-calling assistant is equal to or an
ancestor of the input's accepted parent. Inputs accepted at root, above the
assistant, or on unrelated sibling branches materialize immediately and are never
drained by that round. Applicable agent messages and extension message facts drain
together in durable acceptance order after the complete `ToolResults` aggregate,
while remaining distinct typed domains. Navigation remains allowed, and later
node-producing tool-result events retain the existing durable-head advance
behavior. An ordinary provider inference already in flight does not add another
deferral boundary: minimal commit ordering applies. The outbound assistant
projection remains provider-visible after the sender's message-tool result.

Provider context is rendered from the typed canonical fact. A local inbound
ordinary message keeps the escaped, stable-sender-labelled `[tau-internal]`
wrapper and `<message>` boundary used by live delivery. Cross-session inbound,
watch, and outbound projections retain their distinct typed forms. Initial or
redundant structured watch snapshots produce no provider block and do not
activate the model.

Live activating receives create one runtime-only, payload-free wake keyed by
durable event sequence. A selected-branch inference checkpoint acknowledges a
wake only after its projection has materialized on that branch and only through
the checkpoint watermark. Navigation remains allowed: a wake owed on another
branch stays dormant until that branch is reselected and is never
auto-acknowledged on a sibling. Replay reconstructs canonical context but never
recreates a wake, fanout, route authority, or model activation. A crash before a
checkpoint may therefore lose automatic activation while retaining the message
as context.

These rules apply to every received kind. `Message`, `WatchResponse`, and
`WatchPrompt` use ordinary agent-input activation. Noninitial model-visible
`WatchTurnState` and `WatchProviderStatus` changes use isolated watch
activation. Initial and redundant structured watch snapshots have neither a
wake nor a provider block.

Existing authority and delivery boundaries do not change. The typed
`AgentMessageSent` / `AgentMessageReceived` split remains authoritative; local
journals are not transacted together; cross-harness success remains eligible
after the target commits the exact receive occurrence; and ACK loss may cause a
retry to duplicate durable input, model work, and spend. Tau adds no distributed
WAL, restart deduplication, cross-journal transaction, migration, dual-write
path, or body/message-ID heuristic repair. Decodable legacy duplicate wrappers
remain recorded historical facts.

Replay and cold recovery enforce the same global-round and branch-applicability
rules before provider inference. A second tool-bearing provider response is
rejected before persistence and launches no tools. A journal containing
simultaneous open foreground rounds fails closed. Tau adds no persisted schema,
migration, dual-read path, or compatibility heuristic for this invariant.

The complete behavior is specified by
[SPEC-agent-message-delivery](SPEC-agent-message-delivery.md). This decision is
approved under
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md)
and retains the compatibility policy in
[DECISION-no-backward-compatibility](DECISION-no-backward-compatibility.md).

## Rationale

One typed payload authority makes live delivery, replay, provider assembly, and
compaction agree. Sequence-aware deferred materialization preserves the
provider's required call/result adjacency without conflating harness-owned agent
messages with extension facts. Runtime-only wakes preserve live liveness and
checkpoint accounting without creating durable activation that would contradict
the no-replay-wake contract. The narrower ordinary-inference ordering avoids a
second inference transaction and its wider latency and recovery semantics.

One tree-global foreground round avoids deriving exclusive ownership from mutable
branch selection and gives live dispatch, replay validation, and cold repair one
fail-closed invariant. Branch-applicable deferral still preserves unrelated branch
progress without attaching sibling input to the round's eventual result chain.

Keeping the sender-labelled local wrapper preserves attribution and current live
instruction isolation instead of degrading replay to anonymous raw text.
Applying placement to every received kind avoids retaining the same adjacency
defect in watch notifications; suppressing initial/redundant snapshots preserves
their status-only role. Retaining outbound projection keeps documented sender
history after tool closure. Dormant branch ownership preserves explicit
navigation without falsely acknowledging unseen sibling context. Message-ID or
body-based legacy repair cannot distinguish repeated bodies, retries, escaping,
or self-send directions, so recorded history remains untouched.
