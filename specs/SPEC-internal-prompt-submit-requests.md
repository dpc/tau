# SPEC-internal-prompt-submit-requests: Internal prompt submission

## Record justification

Internal prompt submission spans protocol durability defaults, generic peer
authority/interception, extension activation, exact-generation ownership,
harness prompt validation and queueing, transcript facts, and the timer producer.
No component-local artifact can describe the complete contract.

This specification implements the `extension.internal_prompt_submit_request`
part of the request row in
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).
`agent.start_request` remains a separate migration slice.

## Authority and commit

Every authenticated configured extension entry kind, including configured Core,
may author `extension.internal_prompt_submit_request` without a capability.
Unconfigured and socket peers have no authority. Generic Emit captures the
stable configured publisher, exact run-local `ConnectionId`, instance id, and
kind before ordinary same-name interception.

Drop produces no downstream effect. Replacement repeats structural and
authority admission. After commit, the consumer revalidates the complete live
connection generation. A stale generation's request remains observable but
cannot submit a prompt for a successor or disconnected extension.

## Validation and prompt effects

The committed request names a loaded agent, hidden internal text, and an
optional submitter correlation `ctx_id`. The downstream consumer preserves the
existing loaded-agent and session-route validation. An invalid target remains a
committed observation, produces the existing harness rejection diagnostic, and
creates no prompt fact.

An accepted request enters the ordinary per-agent prompt queue as internal text.
It has no user-message class and does not update latest-user-interaction
metadata. The harness publishes canonical `agent.prompt_submitted` or
`agent.prompt_steered` facts through the existing prompt path; steering retains
the request's `ctx_id`.

The request is operational traffic, not an activation declaration. Pre-Ready
requests remain behind the extension activation barrier and execute only after
the source reaches Ready and earlier ordered activation work settles.

## Persistence and first-party producer

The raw request defaults to `persist=false` and is unconditionally excluded from semantic
agent, session, and restore journals for either caller-supplied
`Emit.persist` value. It has no cold replay or current-state synthesis.
Harness-owned prompt facts keep their existing transcript classifications.
`tau-ext-utils` sends timer wake requests with `persist=false`.

The configured-local-extension boundary is documented in
[`SECURITY.md`](../SECURITY.md). Unrelated authority-matrix rows remain outside
this specification.
